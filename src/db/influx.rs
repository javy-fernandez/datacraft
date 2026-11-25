//! InfluxDB 2 driver over its HTTP API (reqwest).
//!
//! The console takes Flux, or InfluxQL (`SELECT`, `SHOW`…) through the v1 compatibility
//! endpoint, which maps each bucket to a database (InfluxDB 2.4+ creates that mapping for every
//! bucket). Buckets are the databases and measurements the tables; their columns are `_time`,
//! the tag keys and the field keys with their types. The data is read-only: points are written
//! with line protocol, which the data editor doesn't produce.
//!
//! The connection's "User" is the organization (name or ID; it can stay empty when the token
//! sees only one) and its "Password" the API token, sent as `Authorization: Token …`.

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use reqwest::header::{ACCEPT, AUTHORIZATION};
use serde_json::{Value, json};

use super::snowflake::http_error;
use super::{Cell, ColumnInfo, ConnectionConfig, Driver, ObjectKind, ResultSet, SchemaInfo, TableInfo};
use crate::i18n::{tr, trf};

/// Time range of the data editor's query when the filter row doesn't give one.
pub const DEFAULT_RANGE: &str = "-1h";
/// Rows per chunk of a chunked InfluxQL response.
const CHUNK_ROWS: &str = "10000";
/// Buckets per page of `/api/v2/buckets`.
const BUCKET_PAGE: usize = 100;

pub struct InfluxSession {
    http: reqwest::Client,
    /// `http(s)://host:port`.
    base: String,
    token: String,
    /// Organization name or ID.
    org: String,
    /// The current bucket: the database of InfluxQL statements and the one the explorer shows.
    bucket: String,
}

/// A `true`/`1`/`yes`/`on` driver property (`tls`, `tls_insecure`).
pub(super) fn flag(cfg: &ConnectionConfig, name: &str) -> bool {
    cfg.property(name).is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes" | "on"))
}

/// The server's address. A scheme typed in the host field (`https://influx.example.com`) wins
/// over "Use TLS".
pub fn base_url(cfg: &ConnectionConfig) -> String {
    let host = cfg.host.trim().trim_end_matches('/');
    let (tls, host) = match host.split_once("://") {
        Some((scheme, rest)) => (scheme.eq_ignore_ascii_case("https"), rest),
        None => (flag(cfg, "tls"), host),
    };
    let host = if host.is_empty() { "localhost" } else { host };
    let port = if cfg.port == 0 { Driver::InfluxDb.default_port() } else { cfg.port };
    format!("{}://{host}:{port}", if tls { "https" } else { "http" })
}

/// A Flux string literal: `"…"` with `\`, `"` and the interpolation `${` escaped.
pub fn flux_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace("${", "\\${"))
}

/// The error InfluxDB answered with (`{"code": "invalid", "message": "…"}`), or the HTTP status.
async fn check(resp: reqwest::Response) -> anyhow::Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    let message = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v["message"].as_str().or_else(|| v["error"].as_str()).map(str::to_string))
        .unwrap_or_else(|| body.trim().chars().take(300).collect());
    Err(anyhow!(trf("HTTP {status} from InfluxDB: {message}", &[("status", &status), ("message", &message)])))
}

impl InfluxSession {
    pub async fn connect(cfg: &ConnectionConfig) -> anyhow::Result<Self> {
        if cfg.password.trim().is_empty() {
            bail!(tr("An API token is required"));
        }
        let mut base = base_url(cfg);
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // No overall timeout: a long query is stopped from the app (dropping the request
            // cancels it on the server).
            .tcp_keepalive(Duration::from_secs(60))
            .user_agent(format!("DataCraft/{}", env!("CARGO_PKG_VERSION")))
            .tls_danger_accept_invalid_certs(flag(cfg, "tls_insecure"));
        // HTTPS through an SSH tunnel: the server's own name (for its certificate), resolved to
        // the tunnel's listener; the URL's port is the listener's.
        if let Some(name) = cfg.tunnel_host.as_deref().filter(|_| base.starts_with("https://")) {
            base = format!("https://{name}:{}", cfg.port);
            builder = builder.resolve(name, std::net::SocketAddr::from(([127, 0, 0, 1], cfg.port)));
        }
        let http = builder.build().map_err(http_error)?;
        let mut session = Self {
            http,
            base,
            token: cfg.password.trim().to_string(),
            org: cfg.user.trim().to_string(),
            bucket: cfg.database.trim().to_string(),
        };
        if session.org.is_empty() {
            session.org = session.only_org().await?;
        }
        // Checks the token and the organization.
        let buckets = session.list_databases().await?;
        if session.bucket.is_empty()
            && let Some(first) = buckets.first()
        {
            session.bucket = first.clone();
        }
        Ok(session)
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.http.get(format!("{}{path}", self.base)).header(AUTHORIZATION, format!("Token {}", self.token))
    }

    /// `orgID` for an ID (16 hex digits), `org` for a name.
    fn org_param(&self) -> (&'static str, &str) {
        let id = self.org.len() == 16 && self.org.bytes().all(|b| b.is_ascii_hexdigit());
        (if id { "orgID" } else { "org" }, &self.org)
    }

    async fn json(&self, req: reqwest::RequestBuilder) -> anyhow::Result<Value> {
        let resp = check(req.send().await.map_err(http_error)?).await?;
        resp.json().await.map_err(http_error)
    }

    /// The organization when the token sees exactly one. A token that may not list
    /// organizations (403) needs it typed; other failures (TLS, network, a rejected token) are
    /// reported as they are.
    async fn only_org(&self) -> anyhow::Result<String> {
        let resp = self.get("/api/v2/orgs").send().await.map_err(http_error)?;
        if matches!(resp.status().as_u16(), 403 | 404) {
            bail!(tr("Organization is required"));
        }
        let orgs: Value = check(resp).await?.json().await.map_err(http_error)?;
        match orgs["orgs"].as_array().map(Vec::as_slice) {
            Some([org]) => org["name"].as_str().map(str::to_string).ok_or_else(|| anyhow!(tr("Organization is required"))),
            _ => bail!(tr("Organization is required")),
        }
    }

    pub async fn current_database(&mut self) -> anyhow::Result<String> {
        Ok(self.bucket.clone())
    }

    /// The organization's buckets, without the system ones (`_monitoring`, `_tasks`).
    pub async fn list_databases(&mut self) -> anyhow::Result<Vec<String>> {
        let mut names = Vec::new();
        for page in 0.. {
            let offset = (page * BUCKET_PAGE).to_string();
            let limit = BUCKET_PAGE.to_string();
            let req = self.get("/api/v2/buckets").query(&[self.org_param(), ("limit", &limit), ("offset", &offset)]);
            let body = self.json(req).await?;
            let buckets = body["buckets"].as_array().cloned().unwrap_or_default();
            names.extend(buckets.iter().filter(|b| b["type"] != "system").filter_map(|b| b["name"].as_str().map(str::to_string)));
            if buckets.len() < BUCKET_PAGE {
                break;
            }
        }
        if !self.bucket.is_empty() && !names.contains(&self.bucket) {
            names.push(self.bucket.clone());
        }
        names.sort();
        Ok(names)
    }

    pub async fn use_database(&mut self, db: &str) -> anyhow::Result<()> {
        if !db.is_empty() {
            self.bucket = db.to_string();
        }
        Ok(())
    }

    pub async fn server_version(&mut self) -> Option<String> {
        let health = self.json(self.get("/health")).await.ok()?;
        let version = health["version"].as_str()?;
        Some(format!("InfluxDB {}", version.trim_start_matches('v')))
    }

    /// The current bucket's measurements, with `_time`, their tag keys and their field keys.
    pub async fn introspect(&mut self) -> anyhow::Result<Vec<SchemaInfo>> {
        let tables = if self.bucket.is_empty() {
            Vec::new()
        } else {
            match self.v1_schema().await {
                Ok(tables) if !tables.is_empty() => tables,
                // No v1 mapping for the bucket (InfluxDB before 2.4, a bucket named `db/rp`…):
                // just the measurement names, through Flux. Without a mapping SHOW MEASUREMENTS
                // answers with none rather than an error.
                _ => self.flux_measurements().await?,
            }
        };
        Ok(vec![SchemaInfo { name: self.bucket.clone(), tables }])
    }

    /// One InfluxQL request: measurements, tag keys and field keys (with types) of the bucket.
    /// These read the index, without a time range (SHOW MEASUREMENTS and SHOW FIELD KEYS take none).
    async fn v1_schema(&self) -> anyhow::Result<Vec<TableInfo>> {
        let statements = self.influxql_raw("SHOW MEASUREMENTS; SHOW TAG KEYS; SHOW FIELD KEYS", usize::MAX).await?;
        let rows = |i: usize| -> Vec<(String, Vec<Cell>)> {
            statements
                .get(i)
                .into_iter()
                .flat_map(|st| &st.series)
                .flat_map(|s| s.values.iter().map(|v| (s.name.clone(), v.iter().map(cell).collect())))
                .collect()
        };
        let first = |row: &[Cell]| row.first().cloned().flatten().unwrap_or_default();
        let mut tables: Vec<TableInfo> = rows(0)
            .into_iter()
            .map(|(_, row)| TableInfo { name: first(&row), kind: ObjectKind::Table, columns: vec![time_column()] })
            .collect();
        let mut add = |measurement: &str, column: ColumnInfo| {
            if let Some(t) = tables.iter_mut().find(|t| t.name == measurement) {
                t.columns.push(column);
            }
        };
        // Tags and time identify a point; fields may be missing from it.
        for (measurement, row) in rows(1) {
            add(&measurement, ColumnInfo { name: first(&row), data_type: "tag".into(), nullable: false, primary_key: true });
        }
        for (measurement, row) in rows(2) {
            let data_type = row.get(1).cloned().flatten().unwrap_or_else(|| "field".into());
            add(&measurement, ColumnInfo { name: first(&row), data_type, nullable: true, primary_key: false });
        }
        Ok(tables)
    }

    async fn flux_measurements(&self) -> anyhow::Result<Vec<TableInfo>> {
        let script = format!("import \"influxdata/influxdb/schema\"\nschema.measurements(bucket: {})", flux_string(&self.bucket));
        let sets = self.flux(&script, usize::MAX).await?;
        let mut names: Vec<String> = sets
            .iter()
            .filter_map(|rs| rs.columns.iter().position(|c| c == "_value").map(|i| (rs, i)))
            .flat_map(|(rs, i)| rs.rows.iter().filter_map(move |r| r.get(i).cloned().flatten()))
            .collect();
        names.sort();
        names.dedup();
        Ok(names.into_iter().map(|name| TableInfo { name, kind: ObjectKind::Table, columns: vec![time_column()] }).collect())
    }

    /// Runs ONE script: Flux, or an InfluxQL statement against the current bucket.
    pub async fn execute(&mut self, script: &str, max_rows: usize) -> anyhow::Result<Vec<ResultSet>> {
        let start = Instant::now();
        let statement = script.trim().trim_end_matches(';').trim().to_string();
        if statement.is_empty() {
            return Ok(Vec::new());
        }
        let mut results = if is_influxql(&statement) {
            self.influxql_raw(&statement, max_rows).await?.into_iter().map(QlStatement::into_result).collect()
        } else {
            self.flux(&statement, max_rows).await?
        };
        for rs in &mut results {
            rs.statement.clone_from(&statement);
            rs.elapsed = start.elapsed();
        }
        Ok(results)
    }

    /// A Flux query, read as it streams in: once a grid is full the request is dropped, which
    /// cancels the query on the server.
    async fn flux(&self, script: &str, max_rows: usize) -> anyhow::Result<Vec<ResultSet>> {
        let req = self
            .http
            .post(format!("{}/api/v2/query", self.base))
            .header(AUTHORIZATION, format!("Token {}", self.token))
            .header(ACCEPT, "application/csv")
            .query(&[self.org_param()])
            .json(&json!({ "query": script, "type": "flux", "dialect": { "header": true, "annotations": [] } }));
        let mut resp = check(req.send().await.map_err(http_error)?).await?;
        let mut lines = Lines::default();
        let mut tables = FluxTables::new(max_rows);
        loop {
            let chunk = resp.chunk().await.map_err(http_error)?;
            let end = chunk.is_none();
            match chunk {
                Some(bytes) => lines.push(&bytes),
                None => lines.finish(),
            }
            while let Some(line) = lines.next_record(true) {
                if !tables.line(&line)? {
                    return Ok(tables.finish());
                }
            }
            if end {
                return Ok(tables.finish());
            }
        }
    }

    /// InfluxQL statements (`;`-separated) through `/query`, chunked so a large result stops at
    /// `max_rows` without downloading the rest.
    async fn influxql_raw(&self, query: &str, max_rows: usize) -> anyhow::Result<Vec<QlStatement>> {
        if self.bucket.is_empty() {
            bail!(tr("There is no bucket to query"));
        }
        let req = self.get("/query").query(&[
            ("db", self.bucket.as_str()),
            ("q", query),
            ("chunked", "true"),
            ("chunk_size", CHUNK_ROWS),
        ]);
        let mut resp = check(req.send().await.map_err(http_error)?).await?;
        let mut lines = Lines::default();
        let mut results = QlResults::new(max_rows);
        loop {
            let chunk = resp.chunk().await.map_err(http_error)?;
            let end = chunk.is_none();
            match chunk {
                Some(bytes) => lines.push(&bytes),
                None => lines.finish(),
            }
            // Newline-delimited JSON documents (JSON escapes newlines inside strings).
            while let Some(line) = lines.next_record(false) {
                if line.trim().is_empty() {
                    continue;
                }
                let doc: Value = serde_json::from_str(&line)
                    .map_err(|e| anyhow!(trf("Unreadable response from InfluxDB: {error}", &[("error", &e)])))?;
                if !results.add(&doc)? {
                    return Ok(results.statements);
                }
            }
            if end {
                return Ok(results.statements);
            }
        }
    }
}

fn time_column() -> ColumnInfo {
    ColumnInfo { name: "_time".into(), data_type: "time".into(), nullable: false, primary_key: true }
}

fn cell(v: &Value) -> Cell {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

/// Splits a body that arrives in chunks into records: lines, where a CSV line may hold line
/// breaks inside a quoted field.
#[derive(Default)]
struct Lines {
    buf: Vec<u8>,
    /// Bytes of `buf` already scanned for the end of the record.
    scan: usize,
    quoted: bool,
    /// The body ended: what remains is the last record.
    ended: bool,
}

impl Lines {
    fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    fn finish(&mut self) {
        self.ended = true;
    }

    /// The next complete record without its line end (`\n` or `\r\n`). `csv`: double quotes
    /// delimit fields that may span lines.
    fn next_record(&mut self, csv: bool) -> Option<String> {
        while self.scan < self.buf.len() {
            match self.buf[self.scan] {
                b'"' if csv => self.quoted = !self.quoted,
                b'\n' if !self.quoted => {
                    let mut line: Vec<u8> = self.buf.drain(..=self.scan).collect();
                    line.pop();
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    self.scan = 0;
                    return Some(String::from_utf8_lossy(&line).into_owned());
                }
                _ => {}
            }
            self.scan += 1;
        }
        if self.ended && !self.buf.is_empty() {
            let mut line = std::mem::take(&mut self.buf);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.scan = 0;
            return Some(String::from_utf8_lossy(&line).into_owned());
        }
        None
    }
}

/// The fields of one CSV record (RFC 4180: `"…"` with `""` for a quote).
fn split_fields(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// Result sets from Flux's CSV (a header row, no annotations). Each table starts after a blank
/// line with its header, `,result,table,_time,…`: an empty first column, then the yield's name.
/// Consecutive tables of one yield with the same columns share a grid; `table` stays (it tells
/// the series apart) and `result` goes.
struct FluxTables {
    max_rows: usize,
    header: Option<Vec<String>>,
    current: Option<(String, ResultSet)>,
    done: Vec<ResultSet>,
}

impl FluxTables {
    fn new(max_rows: usize) -> Self {
        Self { max_rows, header: None, current: None, done: Vec::new() }
    }

    /// Takes one record; false once the grid being filled is full (reading can stop there).
    fn line(&mut self, line: &str) -> anyhow::Result<bool> {
        if line.is_empty() {
            self.header = None;
            return Ok(true);
        }
        let fields = split_fields(line);
        let Some(header) = &self.header else {
            self.header = Some(fields);
            return Ok(true);
        };
        // An error during the query arrives as a table of its own: `,error,reference`.
        if header.get(1).is_some_and(|h| h == "error") {
            let message = fields.get(1).cloned().unwrap_or_default();
            bail!(trf("InfluxDB: {message}", &[("message", &message)]));
        }
        let has_result = header.get(1).is_some_and(|h| h == "result");
        let skip = if has_result { 2 } else { 1 }.min(header.len());
        let result = if has_result { fields.get(1).cloned().unwrap_or_default() } else { String::new() };
        let columns = &header[skip..];
        if !matches!(&self.current, Some((r, rs)) if *r == result && rs.columns == columns) {
            if let Some((_, rs)) = self.current.take() {
                self.done.push(rs);
            }
            self.current = Some((result, ResultSet { columns: columns.to_vec(), ..Default::default() }));
        }
        let Some((_, rs)) = self.current.as_mut() else { return Ok(true) };
        if rs.rows.len() >= self.max_rows {
            rs.truncated = true;
            return Ok(false);
        }
        let mut row: Vec<Cell> = fields.into_iter().skip(skip).map(|v| (!v.is_empty()).then_some(v)).collect();
        row.resize(rs.columns.len(), None);
        rs.rows.push(row);
        Ok(true)
    }

    /// The grids; a query without rows gives an empty one.
    fn finish(mut self) -> Vec<ResultSet> {
        if let Some((_, rs)) = self.current.take() {
            self.done.push(rs);
        }
        if self.done.is_empty() {
            self.done.push(ResultSet { columns: vec!["result".into()], ..Default::default() });
        }
        self.done
    }
}

#[derive(Default)]
struct QlSeries {
    name: String,
    tags: Vec<(String, String)>,
    columns: Vec<String>,
    values: Vec<Vec<Value>>,
}

#[derive(Default)]
struct QlStatement {
    series: Vec<QlSeries>,
    rows: usize,
    truncated: bool,
}

impl QlStatement {
    /// One grid: the measurement's name when the series come from several, the tags of a
    /// `GROUP BY`, then the columns.
    fn into_result(self) -> ResultSet {
        let several_names = self.series.windows(2).any(|w| w[0].name != w[1].name);
        let mut tags: Vec<String> = self.series.iter().flat_map(|s| s.tags.iter().map(|(k, _)| k.clone())).collect();
        tags.sort();
        tags.dedup();
        let mut columns: Vec<String> = Vec::new();
        for c in self.series.iter().flat_map(|s| &s.columns) {
            if !columns.contains(c) {
                columns.push(c.clone());
            }
        }
        let mut head: Vec<String> = Vec::new();
        if several_names {
            head.push("name".into());
        }
        head.extend(tags.iter().cloned());
        head.extend(columns.iter().cloned());
        if head.is_empty() {
            head.push("result".into());
        }
        let mut rows = Vec::with_capacity(self.rows);
        for s in &self.series {
            let index: Vec<Option<usize>> = columns.iter().map(|c| s.columns.iter().position(|x| x == c)).collect();
            for v in &s.values {
                let mut row: Vec<Cell> = Vec::with_capacity(head.len());
                if several_names {
                    row.push(Some(s.name.clone()));
                }
                row.extend(tags.iter().map(|k| s.tags.iter().find(|(t, _)| t == k).map(|(_, v)| v.clone())));
                row.extend(index.iter().map(|i| i.and_then(|i| v.get(i)).and_then(cell)));
                rows.push(row);
            }
        }
        ResultSet { columns: head, rows, truncated: self.truncated, ..Default::default() }
    }
}

/// An InfluxQL error. Before 2.4 a bucket has no v1 mapping unless one is created, and the
/// server only says the database (or its retention policy) doesn't exist.
fn ql_error(message: &str) -> anyhow::Error {
    if message.starts_with("database not found") || message.starts_with("default retention policy not set") {
        return anyhow!(trf(
            "InfluxDB: {message}. InfluxQL needs a database/retention policy mapping for the bucket (created automatically since InfluxDB 2.4; before, with `influx v1 dbrp create`); Flux needs none.",
            &[("message", &message)]
        ));
    }
    anyhow!(trf("InfluxDB: {message}", &[("message", &message)]))
}

/// The statements of a chunked `/query` response. A series longer than a chunk continues in
/// the next one (`"partial": true`), with the same name and tags.
struct QlResults {
    max_rows: usize,
    statements: Vec<QlStatement>,
}

impl QlResults {
    fn new(max_rows: usize) -> Self {
        Self { max_rows, statements: Vec::new() }
    }

    /// Takes one JSON document; false once a statement reached `max_rows`.
    fn add(&mut self, doc: &Value) -> anyhow::Result<bool> {
        if let Some(e) = doc["error"].as_str() {
            return Err(ql_error(e));
        }
        for r in doc["results"].as_array().into_iter().flatten() {
            if let Some(e) = r["error"].as_str() {
                return Err(ql_error(e));
            }
            let id = r["statement_id"].as_u64().unwrap_or(0) as usize;
            if self.statements.len() <= id {
                self.statements.resize_with(id + 1, QlStatement::default);
            }
            let st = &mut self.statements[id];
            for s in r["series"].as_array().into_iter().flatten() {
                let name = s["name"].as_str().unwrap_or_default().to_string();
                let mut tags: Vec<(String, String)> = s["tags"]
                    .as_object()
                    .map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string())).collect())
                    .unwrap_or_default();
                tags.sort();
                let columns: Vec<String> =
                    s["columns"].as_array().into_iter().flatten().map(|c| c.as_str().unwrap_or_default().to_string()).collect();
                let continues = st.series.last().is_some_and(|l| l.name == name && l.tags == tags && l.columns == columns);
                if !continues {
                    st.series.push(QlSeries { name, tags, columns, values: Vec::new() });
                }
                let Some(target) = st.series.last_mut() else { continue };
                for v in s["values"].as_array().into_iter().flatten() {
                    if st.rows >= self.max_rows {
                        st.truncated = true;
                        return Ok(false);
                    }
                    target.values.push(v.as_array().cloned().unwrap_or_default());
                    st.rows += 1;
                }
            }
        }
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// Language
// ---------------------------------------------------------------------------

/// InfluxQL's statements. A Flux script never starts with one of these words followed by a
/// space (its calls take `(` and its assignments `=`).
const INFLUXQL: [&str; 10] = ["select", "show", "explain", "drop", "delete", "create", "alter", "grant", "revoke", "kill"];

/// The text after leading blanks and `//` / `--` comment lines.
fn skip_comments(s: &str) -> &str {
    let mut s = s.trim_start();
    while s.starts_with("//") || s.starts_with("--") {
        s = s.split_once('\n').map_or("", |(_, rest)| rest).trim_start();
    }
    s
}

fn first_word(s: &str) -> (&str, &str) {
    let s = skip_comments(s);
    let end = s.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(s.len());
    s.split_at(end)
}

/// true for an InfluxQL statement (sent to `/query`), false for Flux.
pub fn is_influxql(statement: &str) -> bool {
    let (word, rest) = first_word(statement);
    INFLUXQL.iter().any(|k| word.eq_ignore_ascii_case(k))
        && rest.starts_with(|c: char| c.is_whitespace())
        // `show = 1` assigns a Flux variable.
        && !rest.trim_start().starts_with('=')
}

/// Packages whose functions send data out of the query: to other servers, databases or
/// notification endpoints, or to InfluxDB's API.
fn sends_data(package: &str) -> bool {
    package.starts_with("contrib/")
        || matches!(
            package,
            "http"
                | "http/requests"
                | "experimental/http"
                | "experimental/http/requests"
                | "experimental/influxdb"
                | "influxdata/influxdb/monitor"
                | "mqtt"
                | "experimental/mqtt"
                | "pagerduty"
                | "slack"
                | "sql"
        )
}

#[derive(Debug, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Punct(char),
}

/// Flux's identifiers, strings and punctuation, without comments and regular expressions.
fn flux_tokens(script: &str) -> Vec<Tok> {
    let mut out: Vec<Tok> = Vec::new();
    let mut chars = script.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {}
            '/' if chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            // A regular expression follows `=~` / `!~`, and may hold quotes.
            '/' if out.last() == Some(&Tok::Punct('~')) => {
                while let Some(c) = chars.next() {
                    match c {
                        '\\' => {
                            chars.next();
                        }
                        '/' => break,
                        _ => {}
                    }
                }
            }
            '"' => {
                let mut s = String::new();
                while let Some(c) = chars.next() {
                    match c {
                        '\\' => s.extend(chars.next()),
                        '"' => break,
                        _ => s.push(c),
                    }
                }
                out.push(Tok::Str(s));
            }
            c if c.is_alphanumeric() || c == '_' => {
                let mut s = String::from(c);
                while let Some(&n) = chars.peek().filter(|n| n.is_alphanumeric() || **n == '_') {
                    s.push(n);
                    chars.next();
                }
                out.push(Tok::Ident(s));
            }
            c => out.push(Tok::Punct(c)),
        }
    }
    out
}

/// true if the script only reads: InfluxQL `SELECT` (without `INTO`) / `SHOW` / `EXPLAIN`, or
/// Flux that neither writes points (`to()`, `wideTo()`) nor imports a package that sends data.
pub fn is_read_command(statement: &str) -> bool {
    if is_influxql(statement) {
        let (word, rest) = first_word(statement);
        let into = flux_tokens(rest).iter().any(|t| matches!(t, Tok::Ident(w) if w.eq_ignore_ascii_case("into")));
        return ["select", "show", "explain"].iter().any(|k| word.eq_ignore_ascii_case(k)) && !into;
    }
    let tokens = flux_tokens(statement);
    // The names the imported packages go by: `import "experimental"` → `experimental`,
    // `import e "experimental"` → `e`.
    let packages: Vec<&str> = tokens
        .iter()
        .enumerate()
        .filter_map(|(i, t)| match (t, tokens.get(i + 1), tokens.get(i + 2)) {
            (Tok::Ident(w), Some(Tok::Ident(alias)), Some(Tok::Str(_))) if w == "import" => Some(alias.as_str()),
            (Tok::Ident(w), Some(Tok::Str(p)), _) if w == "import" => p.rsplit('/').next(),
            _ => None,
        })
        .collect();
    !tokens.iter().enumerate().any(|(i, t)| match t {
        // A call, and also a reference that calls it under another name (`f = to`, `fn: to`,
        // `f = experimental.to`). `to:` is a parameter of other functions and `r.to` a column.
        Tok::Ident(w) if w == "to" || w == "wideTo" => match tokens.get(i + 1) {
            Some(Tok::Punct('(')) => true,
            Some(Tok::Punct(':')) => false,
            _ if i > 0 && tokens[i - 1] == Tok::Punct('.') => {
                matches!(i.checked_sub(2).and_then(|j| tokens.get(j)), Some(Tok::Ident(p)) if packages.contains(&p.as_str()))
            }
            _ => true,
        },
        // `import "http"` or, with an alias, `import h "http"`.
        Tok::Ident(w) if w == "import" => match (tokens.get(i + 1), tokens.get(i + 2)) {
            (Some(Tok::Str(p)), _) | (Some(Tok::Ident(_)), Some(Tok::Str(p))) => sends_data(p),
            _ => false,
        },
        _ => false,
    })
}

/// The data editor's Flux for a measurement: its points in the range from the filter row's
/// right side (`-1h` by default; a start such as `-7d` or `2026-10-01T00:00:00Z`, or
/// `start: …, stop: …`), one row per series and time with a column per field, narrowed by the
/// predicate on `r` from the left side (`r.host == "web-1" and r.usage > 50`), newest first.
pub fn table_query(bucket: &str, measurement: &str, filter: &str, range: &str, max_rows: usize) -> String {
    let range = range.trim();
    let range = if range.is_empty() {
        format!("start: {DEFAULT_RANGE}")
    } else if range.starts_with("start") {
        range.to_string()
    } else {
        format!("start: {range}")
    };
    let mut q = format!(
        "from(bucket: {})\n  |> range({range})\n  |> filter(fn: (r) => r._measurement == {})\n  |> pivot(rowKey: [\"_time\"], columnKey: [\"_field\"], valueColumn: \"_value\")\n",
        flux_string(bucket),
        flux_string(measurement)
    );
    let filter = filter.trim();
    if !filter.is_empty() {
        q.push_str(&format!("  |> filter(fn: (r) => {filter})\n"));
    }
    q.push_str(&format!(
        "  |> drop(columns: [\"_start\", \"_stop\", \"_measurement\"])\n  |> group()\n  |> sort(columns: [\"_time\"], desc: true)\n  |> limit(n: {max_rows})"
    ));
    q
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records(chunks: &[&str]) -> Vec<String> {
        let mut lines = Lines::default();
        let mut out = Vec::new();
        for c in chunks {
            lines.push(c.as_bytes());
            while let Some(l) = lines.next_record(true) {
                out.push(l);
            }
        }
        lines.finish();
        while let Some(l) = lines.next_record(true) {
            out.push(l);
        }
        out
    }

    #[test]
    fn csv_records_survive_chunk_boundaries_and_quoted_line_breaks() {
        let body = ",result,table,_value\r\n,_result,0,\"a, \"\"b\"\"\r\nc\"\r\n\r\n,result,table,x\r\n,_result,1,2";
        let whole = records(&[body]);
        assert_eq!(whole, [",result,table,_value", ",_result,0,\"a, \"\"b\"\"\r\nc\"", "", ",result,table,x", ",_result,1,2"]);
        // The same body cut at every byte gives the same records.
        for cut in 1..body.len() {
            assert_eq!(records(&[&body[..cut], &body[cut..]]), whole, "cut at {cut}");
        }
        assert_eq!(split_fields(&whole[1]), ["", "_result", "0", "a, \"b\"\r\nc"]);
    }

    fn flux(body: &str, max_rows: usize) -> anyhow::Result<Vec<ResultSet>> {
        let mut t = FluxTables::new(max_rows);
        for line in body.split('\n') {
            if !t.line(line.trim_end_matches('\r'))? {
                break;
            }
        }
        Ok(t.finish())
    }

    #[test]
    fn flux_tables_share_a_grid_per_yield_and_columns() {
        let body = ",result,table,_time,_value,host\n,_result,0,t1,1,a\n\n,result,table,_time,_value,host\n,_result,1,t2,,b\n\n\
                    ,result,table,_value\n,_result,2,x\n\n,result,table,_value\n,n,0,5\n";
        let sets = flux(body, 100).unwrap();
        assert_eq!(sets.len(), 3);
        assert_eq!(sets[0].columns, ["table", "_time", "_value", "host"]);
        assert_eq!(sets[0].rows, [vec![Some("0".into()), Some("t1".into()), Some("1".into()), Some("a".into())], vec![
            Some("1".into()),
            Some("t2".into()),
            None,
            Some("b".into())
        ]]);
        assert_eq!(sets[1].rows.len(), 1);
        // Another yield with the same columns is a grid of its own.
        assert_eq!(sets[2].rows, [vec![Some("0".into()), Some("5".into())]]);
    }

    #[test]
    fn flux_stops_at_max_rows_and_reports_errors() {
        let sets = flux(",result,table,v\n,_result,0,1\n,_result,0,2\n,_result,0,3\n", 2).unwrap();
        assert_eq!((sets[0].rows.len(), sets[0].truncated), (2, true));
        let err = flux(",error,reference\n,\"runtime error: boom\",\n", 10).unwrap_err();
        assert!(err.to_string().contains("runtime error: boom"), "{err}");
        // No rows at all: an empty grid.
        let empty = flux("", 10).unwrap();
        assert_eq!((empty.len(), empty[0].rows.len()), (1, 0));
    }

    #[test]
    fn influxql_series_become_one_grid() {
        let mut r = QlResults::new(100);
        // GROUP BY host, the second series split over two chunks.
        let chunk1 = json!({"results": [{"statement_id": 0, "series": [
            {"name": "cpu", "tags": {"host": "a"}, "columns": ["time", "last"], "values": [["t1", 1.5]]},
            {"name": "cpu", "tags": {"host": "b"}, "columns": ["time", "last"], "values": [["t1", 2]], "partial": true}
        ], "partial": true}]});
        let chunk2 = json!({"results": [{"statement_id": 0, "series": [
            {"name": "cpu", "tags": {"host": "b"}, "columns": ["time", "last"], "values": [["t2", null]]}
        ]}]});
        assert!(r.add(&chunk1).unwrap() && r.add(&chunk2).unwrap());
        let rs = r.statements.remove(0).into_result();
        assert_eq!(rs.columns, ["host", "time", "last"]);
        let s = |v: &str| Some(v.to_string());
        assert_eq!(rs.rows, [vec![s("a"), s("t1"), s("1.5")], vec![s("b"), s("t1"), s("2")], vec![s("b"), s("t2"), None]]);

        // SHOW TAG KEYS: one series per measurement, so the name is a column.
        let mut r = QlResults::new(1);
        let doc = json!({"results": [{"statement_id": 0, "series": [
            {"name": "cpu", "columns": ["tagKey"], "values": [["host"], ["region"]]},
            {"name": "log", "columns": ["tagKey"], "values": [["app"]]}
        ]}]});
        assert!(!r.add(&doc).unwrap(), "stops at max_rows");
        let rs = r.statements.remove(0).into_result();
        assert_eq!((rs.columns.as_slice(), rs.rows.len(), rs.truncated), (["tagKey".to_string()].as_slice(), 1, true));

        let err = QlResults::new(10).add(&json!({"results": [{"statement_id": 0, "error": "not implemented: SELECT INTO"}]})).unwrap_err();
        assert!(err.to_string().contains("SELECT INTO"));
    }

    #[test]
    fn tells_influxql_from_flux() {
        assert!(is_influxql("SELECT * FROM cpu"));
        assert!(is_influxql("-- last values\nshow measurements"));
        assert!(!is_influxql("from(bucket: \"b\") |> range(start: -1h)"));
        assert!(!is_influxql("import \"influxdata/influxdb/schema\"\nschema.measurements(bucket: \"b\")"));
        assert!(!is_influxql("show = 1\nshow"));
        assert!(!is_influxql("// select x\nbuckets()"));
    }

    #[test]
    fn read_only_scripts() {
        assert!(is_read_command("from(bucket: \"b\") |> range(start: -1h) |> filter(fn: (r) => r.to == \"x\")"));
        assert!(is_read_command("SHOW TAG KEYS"));
        assert!(is_read_command("select * from cpu"));
        assert!(!is_read_command("select usage into cpu2 from cpu"));
        assert!(!is_read_command("DROP MEASUREMENT cpu"));
        assert!(!is_read_command("from(bucket: \"a\") |> range(start: -1h) |> to(bucket: \"b\")"));
        assert!(!is_read_command("import \"experimental\"\nfrom(bucket: \"a\") |> range(start: -1h) |> experimental.to (bucket: \"b\")"));
        assert!(!is_read_command("import \"influxdata/influxdb\"\ninfluxdb.wideTo(bucket: \"b\")"));
        assert!(!is_read_command("import h \"http\"\nh.post(url: \"http://x\")"));
        assert!(!is_read_command("import \"contrib/sranka/telegram\""));
        // `to` called under another name.
        assert!(!is_read_command("f = to\nfrom(bucket: \"a\") |> range(start: -1h) |> f(bucket: \"b\")"));
        assert!(!is_read_command("w = (tables=<-, fn) => tables |> fn(bucket: \"b\")\nfrom(bucket: \"a\") |> w(fn: to)"));
        assert!(!is_read_command("import e \"experimental\"\nf = e.to\nf(bucket: \"b\")"));
        assert!(!is_read_command("import \"influxdata/influxdb\"\nf = influxdb.wideTo"));
        // `to:` as a parameter and `r.to` as a column only read.
        assert!(is_read_command("import \"experimental\"\nexperimental.addDuration(d: 1h, to: now())"));
        assert!(is_read_command("from(bucket: \"b\") |> range(start: -1h) |> map(fn: (r) => ({r with x: r.to}))"));
        // Strings, comments and regular expressions don't count.
        assert!(is_read_command("// |> to(bucket: \"b\")\nfrom(bucket: \"to(\") |> range(start: -1h) |> filter(fn: (r) => r.host =~ /\"to(/)"));
    }

    #[test]
    fn editor_query() {
        assert_eq!(flux_string("a\"b\\c${d}"), r#""a\"b\\c\${d}""#);
        let q = table_query("metrics", "cpu", "r.host == \"web-1\"", "", 100);
        assert_eq!(
            q,
            "from(bucket: \"metrics\")\n  |> range(start: -1h)\n  |> filter(fn: (r) => r._measurement == \"cpu\")\n  \
             |> pivot(rowKey: [\"_time\"], columnKey: [\"_field\"], valueColumn: \"_value\")\n  |> filter(fn: (r) => r.host == \"web-1\")\n  \
             |> drop(columns: [\"_start\", \"_stop\", \"_measurement\"])\n  |> group()\n  |> sort(columns: [\"_time\"], desc: true)\n  |> limit(n: 100)"
        );
        assert!(table_query("b", "m", "", "-7d", 5).contains("|> range(start: -7d)\n"));
        assert!(table_query("b", "m", "", "start: -2d, stop: -1d", 5).contains("|> range(start: -2d, stop: -1d)\n"));
    }

    #[test]
    fn server_address() {
        let mut cfg = ConnectionConfig::new(1, Driver::InfluxDb);
        assert_eq!(base_url(&cfg), "http://localhost:8086");
        cfg.set_property("tls", "true");
        cfg.host = "influx.example.com/".into();
        assert_eq!(base_url(&cfg), "https://influx.example.com:8086");
        cfg.set_property("tls", "");
        cfg.host = "HTTPS://influx.example.com".into();
        cfg.port = 443;
        assert_eq!(base_url(&cfg), "https://influx.example.com:443");
    }
}

/// Against a live server: DATACRAFT_INFLUX_URL (`http://127.0.0.1:8086`), DATACRAFT_INFLUX_TOKEN
/// (allowed to create and delete buckets) and, optionally, DATACRAFT_INFLUX_ORG. Each test works
/// in a bucket of its own, deleted at the end.
#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::db::Session;

    fn test_cfg() -> Option<ConnectionConfig> {
        let url = std::env::var("DATACRAFT_INFLUX_URL").ok()?;
        let mut cfg = ConnectionConfig::new(1, Driver::InfluxDb);
        assert!(crate::db::url::apply_jdbc_url(&mut cfg, &url), "{url}");
        cfg.password = std::env::var("DATACRAFT_INFLUX_TOKEN").ok()?;
        cfg.user = std::env::var("DATACRAFT_INFLUX_ORG").unwrap_or_default();
        Some(cfg)
    }

    /// A bucket with `cpu` and `log` points from the last minutes, plus `big` points when asked.
    struct Fixture {
        cfg: ConnectionConfig,
        http: reqwest::Client,
        bucket_id: String,
    }

    impl Fixture {
        async fn new(big: usize) -> Option<Self> {
            let mut cfg = test_cfg()?;
            let s = InfluxSession::connect(&cfg).await.unwrap();
            let orgs = s.json(s.get("/api/v2/orgs").query(&[s.org_param()])).await.unwrap();
            let org_id = orgs["orgs"][0]["id"].as_str().unwrap().to_string();
            cfg.database = format!("dc_test_{}", uuid::Uuid::new_v4().simple());
            let created = s
                .json(s.http.post(format!("{}/api/v2/buckets", s.base)).header(AUTHORIZATION, format!("Token {}", s.token)).json(
                    &json!({ "orgID": org_id, "name": cfg.database, "retentionRules": [] }),
                ))
                .await
                .unwrap();
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
            let mut lines = vec![
                format!("cpu,host=web-1,region=eu usage=12.5,cores=8i,ok=true {}", now - 120),
                format!("cpu,host=web-1,region=eu usage=13.25,cores=8i,ok=false {}", now - 60),
                format!("cpu,host=web\\ 2,region=us usage=99.0,cores=16i,ok=true {}", now - 60),
                format!("log,app=api msg=\"a, \\\"quoted\\\" value\",level=\"warn\" {}", now - 30),
            ];
            lines.extend((0..big).map(|i| format!("big,series=s{} v={i}i {}", i % 50, now - 3000 + (i as u64 / 50))));
            let resp = s
                .http
                .post(format!("{}/api/v2/write", s.base))
                .header(AUTHORIZATION, format!("Token {}", s.token))
                .query(&[("orgID", org_id.as_str()), ("bucket", &cfg.database), ("precision", "s")])
                .body(lines.join("\n"))
                .send()
                .await
                .unwrap();
            check(resp).await.unwrap();
            Some(Self { cfg, http: s.http, bucket_id: created["id"].as_str().unwrap().to_string() })
        }

        async fn session(&self) -> InfluxSession {
            InfluxSession::connect(&self.cfg).await.unwrap()
        }

        async fn drop_bucket(self) {
            let base = base_url(&self.cfg);
            let resp = self
                .http
                .delete(format!("{base}/api/v2/buckets/{}", self.bucket_id))
                .header(AUTHORIZATION, format!("Token {}", self.cfg.password))
                .send()
                .await
                .unwrap();
            check(resp).await.unwrap();
        }
    }

    /// InfluxQL works on the bucket: InfluxDB 2.4+ maps every bucket for v1 by itself.
    async fn has_v1(s: &InfluxSession) -> bool {
        // Not SHOW MEASUREMENTS: without a mapping it answers with none instead of failing.
        s.influxql_raw("SHOW TAG KEYS", 1).await.is_ok()
    }

    fn column<'a>(rs: &'a ResultSet, name: &str) -> Vec<Option<&'a str>> {
        let i = rs.columns.iter().position(|c| c == name).unwrap_or_else(|| panic!("no {name} in {:?}", rs.columns));
        rs.rows.iter().map(|r| r[i].as_deref()).collect()
    }

    #[tokio::test]
    #[ignore]
    async fn explorer() {
        let Some(f) = Fixture::new(0).await else { return };
        let mut s = f.session().await;
        let buckets = s.list_databases().await.unwrap();
        assert!(buckets.contains(&f.cfg.database), "{buckets:?}");
        assert!(!buckets.iter().any(|b| b.starts_with('_')), "system buckets: {buckets:?}");
        assert!(s.server_version().await.unwrap().starts_with("InfluxDB 2."));

        let schema = s.introspect().await.unwrap();
        assert_eq!(schema[0].name, f.cfg.database);
        let describe = |name: &str| -> Vec<String> {
            let t = schema[0].tables.iter().find(|t| t.name == name).unwrap();
            t.columns.iter().map(|c| format!("{} {}{}", c.name, c.data_type, if c.primary_key { " key" } else { "" })).collect()
        };
        if has_v1(&s).await {
            assert_eq!(describe("cpu"), ["_time time key", "host tag key", "region tag key", "cores integer", "ok boolean", "usage float"]);
            assert_eq!(describe("log"), ["_time time key", "app tag key", "level string", "msg string"]);
        } else {
            // Before 2.4, without a mapping: the names, through Flux.
            assert_eq!(describe("cpu"), ["_time time key"]);
        }

        // Without a v1 mapping the names still come, through Flux.
        let names: Vec<String> = s.flux_measurements().await.unwrap().into_iter().map(|t| t.name).collect();
        assert_eq!(names, ["cpu", "log"]);
        f.drop_bucket().await;
    }

    #[tokio::test]
    #[ignore]
    async fn flux_and_influxql() {
        let Some(f) = Fixture::new(0).await else { return };
        let mut s = f.session().await;
        let b = flux_string(&f.cfg.database);

        // Two yields: a grid each; quotes and commas inside a string survive.
        let script = format!(
            "from(bucket: {b}) |> range(start: -1h) |> filter(fn: (r) => r._field == \"usage\") |> last() |> keep(columns: [\"host\", \"_value\"]) |> yield(name: \"u\")\n\
             from(bucket: {b}) |> range(start: -1h) |> filter(fn: (r) => r._field == \"msg\") |> keep(columns: [\"_value\"]) |> yield(name: \"m\")"
        );
        let sets = s.execute(&script, 100).await.unwrap();
        assert_eq!(sets.len(), 2, "{:?}", sets.iter().map(|r| &r.columns).collect::<Vec<_>>());
        let (usage, msg) = if sets[0].columns.contains(&"host".to_string()) { (&sets[0], &sets[1]) } else { (&sets[1], &sets[0]) };
        let mut hosts: Vec<_> = column(usage, "host").into_iter().zip(column(usage, "_value")).collect();
        hosts.sort();
        assert_eq!(hosts, [(Some("web 2"), Some("99")), (Some("web-1"), Some("13.25"))]);
        assert_eq!(column(msg, "_value"), [Some("a, \"quoted\" value")]);
        assert!(sets.iter().all(|rs| rs.statement == script));

        // InfluxQL against the current bucket, one row per GROUP BY series.
        let ql = s.execute("SELECT last(usage) AS usage FROM cpu WHERE time > now() - 1h GROUP BY host", 100).await;
        if has_v1(&s).await {
            let rs = ql.unwrap().remove(0);
            assert_eq!(rs.columns, ["host", "time", "usage"]);
            assert_eq!(column(&rs, "host"), [Some("web 2"), Some("web-1")]);
            let err = s.execute("SELECT usage INTO cpu2 FROM cpu", 10).await.unwrap_err().to_string();
            assert!(err.contains("SELECT INTO"), "{err}");
        } else {
            let err = ql.unwrap_err().to_string();
            assert!(err.contains("influx v1 dbrp create"), "{err}");
        }

        // Errors carry InfluxDB's message.
        let err = s.execute("from(bucket: \"x\" |> range(start: -1h)", 10).await.unwrap_err().to_string();
        assert!(err.contains("HTTP 400") && err.contains("compilation failed"), "{err}");
        let err = s.execute("from(bucket: \"no_such_bucket\") |> range(start: -1h)", 10).await.unwrap_err().to_string();
        assert!(err.contains("no_such_bucket"), "{err}");
        // No rows: an empty grid, not a command.
        let rs = s.execute(&format!("from(bucket: {b}) |> range(start: -1h) |> filter(fn: (r) => r._measurement == \"none\")"), 10).await.unwrap();
        assert!(rs.len() == 1 && rs[0].rows.is_empty() && !rs[0].is_command());
        f.drop_bucket().await;
    }

    #[tokio::test]
    #[ignore]
    async fn large_results_stop_at_max_rows() {
        let Some(f) = Fixture::new(20_000).await else { return };
        let mut s = f.session().await;
        let b = flux_string(&f.cfg.database);
        let rs = s.execute(&format!("from(bucket: {b}) |> range(start: -1h) |> filter(fn: (r) => r._measurement == \"big\")"), 100).await.unwrap();
        assert_eq!((rs.len(), rs[0].rows.len(), rs[0].truncated), (1, 100, true));
        if has_v1(&s).await {
            let rs = s.execute("SELECT * FROM big", 100).await.unwrap().remove(0);
            assert_eq!((rs.rows.len(), rs.truncated), (100, true));
            let rs = s.execute("SELECT count(v) FROM big", 100).await.unwrap().remove(0);
            assert_eq!(column(&rs, "count"), [Some("20000")]);
        }
        f.drop_bucket().await;
    }

    #[tokio::test]
    #[ignore]
    async fn data_editor_and_dispatch() {
        let Some(f) = Fixture::new(0).await else { return };
        let mut s = Session::connect(&f.cfg).await.unwrap();
        let q = crate::app::table_query(Driver::InfluxDb, &f.cfg.database, "cpu", "r.host == \"web-1\"", "", 100);
        let rs = s.execute(&q, 100).await.unwrap().remove(0);
        for c in ["_time", "host", "region", "cores", "ok", "usage"] {
            assert!(rs.columns.iter().any(|x| x == c), "{c} in {:?}", rs.columns);
        }
        // Newest first, one row per point of web-1.
        assert_eq!(column(&rs, "usage"), [Some("13.25"), Some("12.5")]);
        assert_eq!(column(&rs, "ok"), [Some("false"), Some("true")]);
        // A range of its own in the filter row's right side.
        let q = crate::app::table_query(Driver::InfluxDb, &f.cfg.database, "cpu", "", "start: -90s", 100);
        assert_eq!(s.execute(&q, 100).await.unwrap()[0].rows.len(), 2);

        assert!(s.apply_changes(&["x".into()]).await.is_err());
        assert!(s.run_transaction(&["x".into()]).await.is_err());
        assert_eq!(s.current_database().await.unwrap(), f.cfg.database);
        f.drop_bucket().await;
    }

    #[tokio::test]
    #[ignore]
    async fn rejects_a_bad_token_or_organization() {
        let Some(mut cfg) = test_cfg() else { return };
        cfg.password = "not-a-token".into();
        // Also when the organization is looked up: the token is the problem.
        cfg.user = String::new();
        let err = InfluxSession::connect(&cfg).await.err().unwrap().to_string();
        assert!(err.contains("HTTP 401"), "{err}");
        cfg.user = "some-org".into();
        let err = InfluxSession::connect(&cfg).await.err().unwrap().to_string();
        assert!(err.contains("HTTP 401"), "{err}");
        let Some(mut cfg) = test_cfg() else { return };
        cfg.user = "no-such-org".into();
        let err = InfluxSession::connect(&cfg).await.err().unwrap().to_string();
        assert!(err.contains("HTTP 404") && err.contains("no-such-org"), "{err}");
    }

    /// HTTPS (DATACRAFT_INFLUX_TLS_URL, an `https://` URL; skipped without it). The certificate
    /// is checked against the system's trust store unless "Trust server certificate" is on: set
    /// DATACRAFT_INFLUX_TLS_TRUSTED=1 when the server's CA is trusted (SSL_CERT_FILE on Linux).
    #[tokio::test]
    #[ignore]
    async fn tls() {
        let (Some(mut cfg), Ok(url)) = (test_cfg(), std::env::var("DATACRAFT_INFLUX_TLS_URL")) else { return };
        assert!(crate::db::url::apply_jdbc_url(&mut cfg, &url), "{url}");
        assert!(flag(&cfg, "tls"), "an https URL turns TLS on");
        let trusted = std::env::var("DATACRAFT_INFLUX_TLS_TRUSTED").is_ok_and(|v| v == "1");
        match InfluxSession::connect(&cfg).await {
            Ok(mut s) => {
                assert!(trusted, "an untrusted certificate was accepted");
                assert!(s.server_version().await.is_some());
            }
            Err(e) => {
                assert!(!trusted, "{e}");
                assert!(e.to_string().to_lowercase().contains("certificate"), "{e}");
            }
        }
        cfg.set_property("tls_insecure", "true");
        let mut s = InfluxSession::connect(&cfg).await.expect("HTTPS without the certificate check");
        assert!(s.server_version().await.is_some());
        // Plain HTTP doesn't talk to the HTTPS port.
        cfg.set_property("tls", "");
        assert!(InfluxSession::connect(&cfg).await.is_err());
    }
}
