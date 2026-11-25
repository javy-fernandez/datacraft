//! Prometheus driver over its HTTP API (reqwest), for Prometheus and the servers that serve the
//! same API under a path prefix: VictoriaMetrics (`/select/0/prometheus` on vmselect), Mimir
//! (`/prometheus`), Thanos, etc.

use super::ConnectionConfig;
use crate::{
    db::{Cell, ObjectKind, ResultSet, SchemaInfo, TableInfo, snowflake::http_error},
    i18n::{tr, trf},
};
use anyhow::{anyhow, bail};
use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::{
    StatusCode,
    header::{
        ACCEPT, AUTHORIZATION, COOKIE, HeaderMap, HeaderName, HeaderValue, PROXY_AUTHORIZATION,
    },
};
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap},
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use tokio::time::Instant;

/// Prometheus' port.
pub const DEFAULT_PORT: u16 = 9090;

/// How requests sign in.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Auth {
    None,
    Basic { user: String, password: String },
    Bearer(String),
}

impl Auth {
    /// User (and password): basic auth; a password alone: a bearer token; neither: none.
    fn from_config(cfg: &ConnectionConfig) -> Self {
        let (user, token) = (cfg.user.trim(), cfg.password.trim());
        if !user.is_empty() {
            Auth::Basic {
                user: user.to_string(),
                password: cfg.password.clone(),
            }
        } else if !token.is_empty() {
            Auth::Bearer(token.to_string())
        } else {
            Auth::None
        }
    }
}

/// Credentials go over HTTPS, or in the clear only to this computer (an SSH tunnel's listener,
/// a `kubectl port-forward`). The host is the one reqwest will connect to (its own parser reads
/// it), and a loopback address is one that parses as such: `127.attacker.example` is a name.
fn sends_securely(base: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(base) else {
        return false;
    };
    if url.scheme() == "https" {
        return true;
    }
    let host = url.host_str().unwrap_or_default();
    let ip = host.trim_start_matches('[').trim_end_matches(']');
    host == "localhost" || ip.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Whether the address carries a user or a password (`http://user:password@host`).
fn has_credentials(base: &str) -> bool {
    reqwest::Url::parse(base)
        .is_ok_and(|url| !url.username().is_empty() || url.password().is_some())
}

/// Driver properties named `header.<Name>` (`header.X-Scope-OrgID` = `tenant-1`), sent with every
/// request. Not the headers that sign in: a property is saved in the clear and would skip the
/// HTTPS check, so those are refused and the password field is named instead.
fn extra_headers(cfg: &ConnectionConfig) -> anyhow::Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for (key, value) in &cfg.properties {
        let Some(raw) = key.trim().strip_prefix("header.") else {
            continue;
        };
        let invalid = || anyhow!(trf("Invalid HTTP header {name}", &[("name", &raw)]));
        let name = HeaderName::from_bytes(raw.trim().as_bytes()).map_err(|_| invalid())?;
        if [AUTHORIZATION, PROXY_AUTHORIZATION, COOKIE].contains(&name) {
            bail!(trf(
                "The {name} header can't be a driver property: use the \"Password or token\" field",
                &[("name", &raw.trim())]
            ));
        }
        let value = HeaderValue::from_str(value.trim()).map_err(|_| invalid())?;
        headers.insert(name, value);
    }
    Ok(headers)
}

/// A successful API answer: its `data`, with the server's warnings and infos.
#[derive(Debug)]
struct Reply {
    data: Value,
    // A partial response, a histogram fallback.
    notices: Vec<String>,
}

/// The API's envelope: `{"status": "success", "data": ..., "warnings": [...], "infos": [...]}` or
/// `{"status": "error", "errorType": ..., "error": ...}`, a proxy's own page otherwise.
fn parse_reply(status: StatusCode, body: &str) -> anyhow::Result<Reply> {
    let json = serde_json::from_str::<Value>(body).ok();
    if let Some(v) = json
        .as_ref()
        .filter(|v| status.is_success() && v["status"] == "success")
    {
        let notices = ["warnings", "infos"]
            .into_iter()
            .flat_map(|k| v[k].as_array().into_iter().flatten())
            .filter_map(|n| n.as_str().map(str::to_string))
            .collect();
        return Ok(Reply {
            data: v["data"].clone(),
            notices,
        });
    }
    let message = match json.as_ref().and_then(|v| v["error"].as_str()) {
        Some(error) => match json.as_ref().and_then(|v| v["errorType"].as_str()) {
            Some(kind) => format!("{kind}: {error}"),
            None => error.to_string(),
        },
        None => body.trim().chars().take(300).collect(),
    };
    Err(anyhow!(trf(
        "HTTP {status} from Prometheus: {message}",
        &[("status", &status), ("message", &message)]
    )))
}

/// An answer is read up to this size (256 MiB): a bigger one is refused, asking for a narrower
/// query, before it takes the memory.
const MAX_RESPONSE_BYTES: usize = 256 * 1024 * 1024;

/// The answer's body, refused once it is over `max` bytes.
async fn read_body(mut resp: reqwest::Response, max: usize) -> anyhow::Result<String> {
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(http_error)? {
        if body.len() + chunk.len() > max {
            bail!(trf(
                "Prometheus's answer is over {size} MB: narrow the query or its range",
                &[("size", &(max / (1024 * 1024)))]
            ));
        }
        body.extend_from_slice(&chunk);
    }
    // JSON is UTF-8: as a rule the bytes become the text without a copy.
    Ok(String::from_utf8(body)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
}

/// The single "database" the explorer shows: the API has none.
pub const DATABASE: &str = "metrics";

pub struct PromSession {
    http: reqwest::Client,
    /// `scheme://host[:port][/prefix]`.
    base: String,
    auth: Auth,
}

impl PromSession {
    pub async fn connect(cfg: &ConnectionConfig) -> anyhow::Result<Self> {
        let auth = Auth::from_config(cfg);
        let mut base = base_url(cfg);
        // `http://user:password@host` would sign in without the HTTPS check, and the address
        // shows up in error messages.
        if has_credentials(&base) {
            bail!(tr(
                "Put the user and password in their fields, not in the address"
            ));
        }
        if auth != Auth::None && !sends_securely(&base) {
            bail!(tr(
                "Signing in to Prometheus needs HTTPS: turn on \"Use TLS\""
            ));
        }
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // No overall timeout: a long query is stopped from the app (dropping the request
            // cancels it on the server).
            .tcp_keepalive(Duration::from_secs(60))
            .user_agent(format!("DataCraft/{}", env!("CARGO_PKG_VERSION")))
            .default_headers(extra_headers(cfg)?)
            .tls_danger_accept_invalid_certs(super::influx::flag(cfg, "tls_insecure"))
            // A redirect could take the token and the `header.*` properties to another host or
            // to plain HTTP; the API never needs one.
            .redirect(reqwest::redirect::Policy::none());
        // HTTPS through an SSH tunnel: the server's own name (from its certificates), resolves to
        // the tunnel's listener.
        // An IP address can't be resolved to the listener (reqwest connects to it directly,
        // around the tunnel), so the address stays 127.0.0.1 and its certificate is checked
        // against that.
        if let Some(name) = cfg
            .tunnel_host
            .as_deref()
            .filter(|_| base.starts_with("https://"))
            .filter(|name| {
                name.trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<IpAddr>()
                    .is_err()
            })
        {
            base = base.replacen("127.0.0.1", name, 1);
            builder = builder.resolve(name, SocketAddr::from(([127, 0, 0, 1], cfg.port)));
        }
        let http = builder.build().map_err(http_error)?;
        let session = Self { http, base, auth };
        // Check the address, the path prefix and the credentials; every server answers it.
        session
            .call(
                session
                    .get("/api/v1/query")
                    .query(&[("query", "vector(1)")]),
            )
            .await?;
        Ok(session)
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        let req = self
            .http
            .get(format!("{}{path}", self.base))
            .header(ACCEPT, "application/json");
        match &self.auth {
            Auth::None => req,
            Auth::Basic { user, password } => req.basic_auth(user, Some(password)),
            Auth::Bearer(token) => req.bearer_auth(token),
        }
    }

    async fn call(&self, req: reqwest::RequestBuilder) -> anyhow::Result<Reply> {
        let resp = req.send().await.map_err(http_error)?;
        let status = resp.status();
        let body = read_body(resp, MAX_RESPONSE_BYTES).await?;
        parse_reply(status, &body)
    }

    pub async fn current_database(&mut self) -> anyhow::Result<String> {
        Ok(DATABASE.to_string())
    }

    pub async fn list_databases(&mut self) -> anyhow::Result<Vec<String>> {
        Ok(vec![DATABASE.to_string()])
    }

    /// "Prometheus 3.5.0". VictoriaMetrics answers with a fixed Prometheus version and no build
    /// details, hence "Prometheus API 2.24.0" there.
    pub async fn server_version(&mut self) -> Option<String> {
        let data = self
            .call(self.get("/api/v1/status/buildinfo"))
            .await
            .ok()?
            .data;
        let version = data["version"].as_str()?;
        let product = if data["revision"].is_string() {
            "Prometheus"
        } else {
            "Prometheus API"
        };
        Some(format!("{product} {version}"))
    }

    pub async fn introspect(&mut self) -> anyhow::Result<Vec<SchemaInfo>> {
        let now = unix_now();
        let req = self
            .get("/api/v1/label/__name__/values")
            .query(&[("start", now - METRICS_WINDOW), ("end", now)])
            .query(&[("limit", MAX_METRICS)]);
        let reply = self.call(req).await?;
        Ok(vec![SchemaInfo {
            name: DATABASE.to_string(),
            tables: metric_tables(&reply.data),
        }])
    }

    /// Runs ONE request: PromQL with `#` directives (see `parse_eval`). The server's warnings
    /// and infos follow as a second result.
    pub async fn execute(
        &mut self,
        script: &str,
        max_rows: usize,
    ) -> anyhow::Result<Vec<ResultSet>> {
        let started = Instant::now();
        let statement = script.trim();
        if statement.is_empty() {
            return Ok(Vec::new());
        }
        let now = unix_now();
        let requ = match parse_eval(statement, now)? {
            Eval::Instant { time } => {
                let req = self.get("/api/v1/query").query(&[("query", statement)]);
                match time {
                    Some(t) => req.query(&[("time", t)]),
                    None => req,
                }
            }
            Eval::Range { start, end, step } => self
                .get("/api/v1/query_range")
                .query(&[("query", statement)])
                .query(&[("start", start), ("end", end), ("step", step)]),
        };
        let reply = self.call(requ).await?;
        let mut rs = result_set(&reply.data, max_rows)?;
        rs.statement = statement.to_string();
        rs.elapsed = started.elapsed();
        let mut result = vec![rs];
        if !reply.notices.is_empty() {
            result.push(ResultSet {
                statement: statement.to_string(),
                columns: vec!["notice".into()],
                rows: reply.notices.into_iter().map(|n| vec![Some(n)]).collect(),
                ..Default::default()
            });
        }
        Ok(result)
    }
}

/// `[timestamp, value]` as two cells: the time in RFC 3339 (UTC, milliseconds) and the value as
/// the API gives it (`"NaN"`, `"+Inf"` ...), or a native histogram as JSON.
fn sample(point: &Value) -> Vec<Cell> {
    let time = point[0]
        .as_f64()
        .and_then(|t| DateTime::from_timestamp_millis((t * 1000.0).round() as i64))
        .map(|d| d.to_rfc3339_opts(SecondsFormat::Millis, true));
    let value = match &point[1] {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    };
    vec![time, value]
}

/// The label names of the series: `__name__` first when a series has it, the others sorted.
fn label_columns(series: &[Value]) -> Vec<String> {
    let mut names = BTreeSet::new();
    for s in series {
        if let Some(metric) = s["metric"].as_object() {
            names.extend(metric.keys().cloned());
        }
    }
    let has_name = names.remove("__name__");
    has_name
        .then(|| "__name__".to_string())
        .into_iter()
        .chain(names)
        .collect()
}

/// The rows of a query's `data` (`{"resultType": ..., "result": ...}`): one per series for an instant
/// vector, one per sample for a range matrix (long format), each with the series' labels, then
/// `timestamp` and `value`; one row for a scalar or a string, `max_rows` 0: no limit.
fn result_set(data: &Value, max_rows: usize) -> anyhow::Result<ResultSet> {
    let max_rows = if max_rows == 0 { usize::MAX } else { max_rows };
    let result = &data["result"];
    let mut rs = ResultSet::default();
    match data["resultType"].as_str().unwrap_or_default() {
        "scalar" | "string" => {
            rs.columns = vec!["timestamp".into(), "value".into()];
            rs.rows.push(sample(result));
        }
        kind @ ("vector" | "matrix") => {
            let series = result.as_array().map(Vec::as_slice).unwrap_or_default();
            let labels = label_columns(series);
            rs.columns = labels
                .iter()
                .cloned()
                .chain(["timestamp", "value"].map(String::from))
                .collect();
            's: for s in series {
                let names: Vec<Cell> = labels
                    .iter()
                    .map(|l| s["metric"][l].as_str().map(str::to_string))
                    .collect();
                let mut points: Vec<&Value> = if kind == "vector" {
                    [&s["value"], &s["histogram"]]
                        .into_iter()
                        .filter(|v| !v.is_null())
                        .collect()
                } else {
                    ["values", "histograms"]
                        .into_iter()
                        .flat_map(|k| s[k].as_array().into_iter().flatten())
                        .collect()
                };
                points.sort_by(|a, b| {
                    a[0].as_f64()
                        .partial_cmp(&b[0].as_f64())
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                for point in points {
                    if rs.rows.len() == max_rows {
                        rs.truncated = true;
                        break 's;
                    }
                    rs.rows
                        .push(names.iter().cloned().chain(sample(point)).collect());
                }
            }
        }
        other => bail!(trf(
            "Unknown result type from Prometheus: {kind}",
            &[("kind", &other)]
        )),
    }
    Ok(rs)
}

/// A Prometheus duration (`500ms`, `90s`, `1h30m`, `2d`, `1w`, `1y`) in seconds, or a bare
/// number of them (`15`, `0.5`), as the API takes a `step`.
fn parse_duration(s: &str) -> Option<f64> {
    let mut rest = s.trim();
    if rest.is_empty() {
        return None;
    }
    if let Ok(seconds) = rest.parse::<f64>() {
        return (seconds.is_finite() && seconds >= 0.0).then_some(seconds);
    }
    let mut total = 0.0;
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        let n: f64 = rest[..digits].parse().ok()?;
        rest = &rest[digits..];
        // `ms` before `m`.
        let unit = ["ms", "s", "m", "h", "d", "w", "y"]
            .into_iter()
            .find(|u| rest.starts_with(u))?;
        rest = &rest[unit.len()..];
        total += n * match unit {
            "ms" => 0.001,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3_600.0,
            "d" => 86_400.0,
            "w" => 604_800.0,
            _ => 31_536_000.0,
        };
    }
    Some(total)
}

/// `now`, `now-1h`, Unix seconds or RFC 3339 (`2026-11-04T12:00:00Z`), in Unix seconds.
fn parse_time(s: &str, now: f64) -> Option<f64> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("now") {
        return match rest.strip_prefix('-') {
            None if rest.is_empty() => Some(now),
            Some(d) => Some(now - parse_duration(d)?),
            _ => None,
        };
    }
    s.parse::<f64>().ok().filter(|t| t.is_finite()).or_else(|| {
        DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|d| d.timestamp_millis() as f64 / 1000.0)
    })
}

/// The keys of `#` directive lines.
const DIRECTIVES: [&str; 5] = ["range", "start", "end", "step", "time"];

/// The directive key of a token: `range:` -> `range`.
fn directive_key(token: &str) -> Option<&str> {
    token.strip_suffix(':').filter(|k| DIRECTIVES.contains(k))
}

/// Points per series of a range without `step:`. The grid shows a row per sample and, by
/// default, 1,000 rows: 250 points leave room for several series (the API's own limit is
/// 11,000).
const AUTO_POINTS: f64 = 250.0;

/// When a request is evaluated, in Unix seconds.
#[derive(Debug, Clone, PartialEq)]
enum Eval {
    /// At `time`; the server's now when None.
    Instant {
        time: Option<f64>,
    },
    Range {
        start: f64,
        end: f64,
        step: f64,
    },
}

/// The request's directives: comment lines that start with a key (`# range: 6h step: 1m`,
/// `# start: now-2h end: now-1h`, `# time: 2026-11-04T12:00:00Z`). A range (`range:` back from
/// `end:` or now, or `start:`...`end:`) makes it a range query, with `step:` or one that gives
/// about 250 points; otherwise it is an instant query at `time:` or now. Other comments are
/// left alone. What looks like a directive and can't be followed is an error, not a comment:
/// a key glued to its value (`# range:6h`), `step:` or `end:` without a range, `time:` with one.
fn parse_eval(request: &str, now: f64) -> anyhow::Result<Eval> {
    let bad = |d: &str| anyhow!(trf("Invalid directive: {directive}", &[("directive", &d)]));
    let mut found: HashMap<&str, &str> = HashMap::new();
    for line in request.lines() {
        let Some(comment) = line.trim().strip_prefix('#') else {
            continue;
        };
        let mut tokens = comment.split_whitespace().peekable();
        let Some(first) = tokens.peek().copied() else {
            continue;
        };
        if directive_key(first).is_none() {
            match first.split_once(':') {
                Some((key, value)) if DIRECTIVES.contains(&key) && !value.is_empty() => {
                    return Err(bad(first));
                }
                _ => continue,
            }
        }
        while let Some(token) = tokens.next() {
            let key = directive_key(token).ok_or_else(|| bad(token))?;
            let value = tokens.next().ok_or_else(|| bad(token))?;
            found.insert(key, value);
        }
    }
    let time = |key: &str| {
        found
            .get(key)
            .map(|v| parse_time(v, now).ok_or_else(|| bad(&format!("{key}: {v}"))))
            .transpose()
    };
    let duration = |key: &str| {
        found
            .get(key)
            .map(|v| {
                parse_duration(v)
                    .filter(|d| *d > 0.0)
                    .ok_or_else(|| bad(&format!("{key}: {v}")))
            })
            .transpose()
    };
    let (range, start) = (duration("range")?, time("start")?);
    let ranged = range.is_some() || start.is_some();
    // `step:` and `end:` belong to a range, `time:` to an instant query.
    let misplaced: &[&str] = if ranged { &["time"] } else { &["step", "end"] };
    if let Some((key, value)) = misplaced.iter().find_map(|k| found.get_key_value(k)) {
        return Err(bad(&format!("{key}: {value}")));
    }
    if !ranged {
        return Ok(Eval::Instant {
            time: time("time")?,
        });
    }
    let end = time("end")?.unwrap_or(now);
    let start = start.unwrap_or_else(|| end - range.unwrap_or_default());
    if start >= end {
        return Err(bad(&format!("start: {start} end: {end}")));
    }
    let step = duration("step")?.unwrap_or_else(|| ((end - start) / AUTO_POINTS).ceil().max(1.0));
    Ok(Eval::Range { start, end, step })
}

/// The requests of a script: blocks of lines separated by blank lines, each with its `#`
/// directives. Blocks with only comments are skipped. Byte ranges, trimmed.
pub fn split_script(script: &str) -> Vec<(usize, usize)> {
    crate::sql::split_blocks(script, "#", false)
}

/// The API's base, `scheme://host[:port][/prefix]` without a trailing slash. The host field takes
/// a host name or a whole URL with a path prefix (`http://vmselect:8481/select/0/prometheus`): its
/// scheme wins over "Use TLS", and a port in it over the port field. The scheme's own port is
/// left out.
pub fn base_url(cfg: &ConnectionConfig) -> String {
    let field = cfg.host.trim().trim_end_matches("/");
    let (tls, rest) = match field.split_once("://") {
        Some((scheme, rest)) => (scheme.eq_ignore_ascii_case("https"), rest),
        None => (super::influx::flag(cfg, "tls"), field),
    };
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let field_port = if cfg.port == 0 {
        DEFAULT_PORT
    } else {
        cfg.port
    };
    // A bare IPv6 address (`fd00::12`) has colons and no port, as in `ssh::host_port`.
    let bare_ipv6 = !authority.starts_with('[') && authority.matches(':').count() > 1;
    let (mut host, port) = if bare_ipv6 {
        (format!("[{authority}]"), field_port)
    } else {
        super::url::parse_authority(authority, field_port)
            .unwrap_or_else(|| (authority.to_string(), field_port))
    };
    if host.is_empty() {
        host = "localhost".to_string();
    };
    let scheme = if tls { "https" } else { "http" };
    match (tls, port) {
        (true, 443) | (false, 80) => format!("{scheme}://{host}{path}"),
        _ => format!("{scheme}://{host}:{port}{path}"),
    }
}

// A metric name as PromQL takes it: as is when it is a plain identifier, else as the quoted
// selector `{"name"}` (Prometheus 3; OTLP metrics often have dots).
pub fn quote_metric(name: &str) -> String {
    let mut chars = name.chars();
    let plain = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == ':')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':');
    if plain {
        name.to_string()
    } else {
        format!("{{{}}}", string_literal(name))
    }
}

// A PromQL string literal: `"..."` with `\` and `"` escaped.
pub fn string_literal(s: &str) -> String {
    format!("\"{}\"", s.replace("\\", "\\\\").replace('"', "\\\""))
}

/// Now, in Unix seconds (milliseconds kept).
fn unix_now() -> f64 {
    Utc::now().timestamp_millis() as f64 / 1000.0
}

/// Metric names the explorer lists at most (a big VictoriaMetrics cluster has hundreds of
/// thousands).
const MAX_METRICS: usize = 10_000;
/// The explorer lists the metrics with samples in this window back from now, in seconds.
const METRICS_WINDOW: f64 = 3_600.0;

/// The explorer's tables: the metrics names of `/api/v1/label/__name__/values`, sorted, at most
/// `MAX_METRICS`. Their labels show when one is opened.
fn metric_tables(data: &Value) -> Vec<TableInfo> {
    let mut names: Vec<&str> = data
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    names.sort_unstable();
    names.dedup();
    names.truncate(MAX_METRICS);
    names
        .into_iter()
        .map(|n| TableInfo {
            name: n.to_string(),
            kind: ObjectKind::Table,
            columns: Vec::new(),
        })
        .collect()
}

/// The data editor's query: the metric's series, narrowed by the filter row's label matches
/// (`job="node", instance=~"web.*"`, braces optional), at their latest sample; with a range in
/// the right side (`15m`, `[1h]`), every raw sample in that window back from now.
pub fn table_query(metric: &str, matchers: &str, range: &str) -> String {
    let matchers = matchers
        .trim()
        .trim_start_matches('{')
        .trim_end_matches('}')
        .trim();
    let plain = quote_metric(metric) == metric;
    let selector = match (plain, matchers.is_empty()) {
        (true, true) => metric.to_string(),
        (true, false) => format!("{metric}{{{matchers}}}"),
        // Names that aren't identifiers go inside the braces (`{"http.server.duration"}`).
        (false, true) => format!("{{{}}}", string_literal(metric)),
        (false, false) => format!("{{{}, {matchers}}}", string_literal(metric)),
    };
    let range = range
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim();
    if range.is_empty() {
        selector
    } else {
        format!("{selector}[{range}]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Driver;
    use reqwest::StatusCode;

    fn cfg(host: &str, port: u16) -> ConnectionConfig {
        let mut c = ConnectionConfig::new(1, Driver::Prometheus);
        c.host = host.into();
        c.port = port;
        c
    }

    fn rows(data: &str, max_rows: usize) -> ResultSet {
        result_set(&serde_json::from_str(data).unwrap(), max_rows).unwrap()
    }

    const T0: &str = "2023-11-14T22:13:20.123Z";

    /// Against live servers, each optional: DATACRAFT_PROMETHEUS_URL, a Prometheus scraping itself
    /// (`http://127.0.0.1:9090`), and DATACRAFT_VM_URL, a single-node VictoriaMetrics started with
    /// `-search.latencyOffset=0s` (`http://127.0.0.1:8428/prometheus`), where the tests import their
    /// own series.
    mod live_tests {
        use super::*;
        use crate::db::{Driver, Session};

        fn live_cfg(var: &str) -> Option<ConnectionConfig> {
            let url = std::env::var(var).ok()?;
            let mut cfg = ConnectionConfig::new(1, Driver::Prometheus);
            assert!(crate::db::url::apply_jdbc_url(&mut cfg, &url), "{url}");
            Some(cfg)
        }

        fn column<'a>(rs: &'a ResultSet, row: usize, name: &str) -> Option<&'a str> {
            let i = rs.columns.iter().position(|c| c == name)?;
            rs.rows.get(row)?.get(i)?.as_deref()
        }

        #[tokio::test]
        #[ignore]
        async fn prometheus_end_to_end() {
            let Some(cfg) = live_cfg("DATACRAFT_PROMETHEUS_URL") else {
                return;
            };
            let mut s = Session::connect(&cfg).await.unwrap();
            let version = s.server_version(Driver::Prometheus).await.unwrap();
            assert!(version.starts_with("Prometheus 3."), "{version}");

            // A fresh server scrapes itself within its first interval.
            let mut up = ResultSet::default();
            for _ in 0..30 {
                up = s.execute("up", 0).await.unwrap().remove(0);
                if !up.rows.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            for c in ["__name__", "job", "instance", "timestamp", "value"] {
                assert!(up.columns.iter().any(|x| x == c), "{c} in {:?}", up.columns);
            }
            assert_eq!(
                (column(&up, 0, "job"), column(&up, 0, "value")),
                (Some("prometheus"), Some("1"))
            );

            let range = s
                .execute("# range: 2m step: 5s\nup", 0)
                .await
                .unwrap()
                .remove(0);
            assert!(!range.rows.is_empty() && !range.truncated);
            assert_eq!(range.columns, up.columns);

            let err = s.execute("1 +", 0).await.unwrap_err().to_string();
            assert!(
                err.starts_with("HTTP 400 Bad Request from Prometheus: bad_data:"),
                "{err}"
            );

            let schemas = s.introspect().await.unwrap();
            assert!(schemas[0].tables.iter().any(|t| t.name == "up"));

            let q = crate::app::table_query(
                Driver::Prometheus,
                "",
                "up",
                r#"job="prometheus""#,
                "1m",
                100,
            );
            assert_eq!(q, r#"up{job="prometheus"}[1m]"#);
            let raw = s.execute(&q, 100).await.unwrap().remove(0);
            assert!(
                raw.rows
                    .iter()
                    .all(|r| r.last().unwrap().as_deref() == Some("1"))
                    && !raw.rows.is_empty()
            );
        }

        /// Writes `dc_temp` (rooms a and b) and a dotted `http.server.duration` at now.
        async fn vm_fixture(cfg: &ConnectionConfig) {
            let base = base_url(cfg);
            let root = base.trim_end_matches("/prometheus");
            let now = chrono::Utc::now().timestamp_millis();
            let lines = [
                (r#"{"__name__":"dc_temp","room":"a"}"#, "21.5"),
                (r#"{"__name__":"dc_temp","room":"b"}"#, "19"),
                (
                    r#"{"__name__":"http.server.duration","code":"200"}"#,
                    "0.25",
                ),
            ]
            .map(|(metric, v)| {
                format!(r#"{{"metric":{metric},"values":[{v}],"timestamps":[{now}]}}"#)
            })
            .join("\n");
            let http = reqwest::Client::new();
            let resp = http
                .post(format!("{root}/api/v1/import"))
                .body(lines)
                .send()
                .await
                .unwrap();
            assert!(resp.status().is_success(), "{}", resp.status());
            http.get(format!("{root}/internal/force_flush"))
                .send()
                .await
                .unwrap();
        }

        #[tokio::test]
        #[ignore]
        async fn victoriametrics_end_to_end() {
            let Some(cfg) = live_cfg("DATACRAFT_VM_URL") else {
                return;
            };
            vm_fixture(&cfg).await;
            let mut s = Session::connect(&cfg).await.unwrap();
            // A fixed Prometheus version and no build details.
            let version = s.server_version(Driver::Prometheus).await.unwrap();
            assert!(version.starts_with("Prometheus API "), "{version}");

            // What was imported shows in queries a moment after the flush (some 2 s on 1.157).
            let dotted_query = table_query("http.server.duration", r#"code="200""#, "");
            let (mut rs, mut dotted) = (ResultSet::default(), ResultSet::default());
            for _ in 0..30 {
                rs = s.execute("dc_temp", 0).await.unwrap().remove(0);
                dotted = s.execute(&dotted_query, 0).await.unwrap().remove(0);
                if rs.rows.len() == 2 && !dotted.rows.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            assert_eq!(rs.columns, ["__name__", "room", "timestamp", "value"]);
            let mut values: Vec<_> = (0..rs.rows.len())
                .filter_map(|i| column(&rs, i, "value"))
                .collect();
            values.sort_unstable();
            assert_eq!(values, ["19", "21.5"]);
            let cut = s.execute("dc_temp", 1).await.unwrap().remove(0);
            assert_eq!((cut.rows.len(), cut.truncated), (1, true));

            // Names that aren't identifiers, through the quoted selector.
            assert_eq!(column(&dotted, 0, "value"), Some("0.25"));

            let schemas = s.introspect().await.unwrap();
            let names: Vec<&str> = schemas[0].tables.iter().map(|t| t.name.as_str()).collect();
            assert!(
                names.contains(&"dc_temp") && names.contains(&"http.server.duration"),
                "{names:?}"
            );

            let err = s.execute("1 +", 0).await.unwrap_err().to_string();
            assert!(
                err.starts_with("HTTP 400 Bad Request from Prometheus: "),
                "{err}"
            );
        }
    }

    #[test]
    fn data_editor_selectors() {
        assert_eq!(table_query("up", "", ""), "up");
        assert_eq!(table_query("up", r#"job="node""#, ""), r#"up{job="node"}"#);
        assert_eq!(
            table_query("up", r#" {job="node"} "#, "5m"),
            r#"up{job="node"}[5m]"#
        );
        assert_eq!(table_query("up", "", "[1h]"), "up[1h]");
        assert_eq!(
            table_query("http.server.duration", "", ""),
            r#"{"http.server.duration"}"#
        );
        assert_eq!(
            table_query("http.server.duration", r#"code="200""#, "15m"),
            r#"{"http.server.duration", code="200"}[15m]"#
        );
    }

    #[test]
    fn durations_and_times() {
        assert_eq!(parse_duration("90s"), Some(90.0));
        assert_eq!(parse_duration("1h30m"), Some(5400.0));
        assert_eq!(parse_duration("500ms"), Some(0.5));
        assert_eq!(parse_duration("2d"), Some(172_800.0));
        assert_eq!(parse_duration("1.5h"), None);
        assert_eq!(parse_duration("soon"), None);
        // A bare number is seconds, as the API takes them.
        assert_eq!(parse_duration("15"), Some(15.0));
        assert_eq!(parse_duration("0.5"), Some(0.5));
        for bad in ["NaN", "inf", "-inf", "-5", ""] {
            assert_eq!(parse_duration(bad), None, "{bad}");
        }
        assert_eq!(parse_time("now", 100.0), Some(100.0));
        assert_eq!(parse_time("now-1m", 100.0), Some(40.0));
        assert_eq!(parse_time("now-90", 100.0), Some(10.0));
        for bad in ["NaN", "inf", "-infinity", "now-inf"] {
            assert_eq!(parse_time(bad, 100.0), None, "{bad}");
        }
        assert_eq!(parse_time("1700000000.5", 0.0), Some(1_700_000_000.5));
        assert_eq!(
            parse_time("2023-11-14T22:13:20Z", 0.0),
            Some(1_700_000_000.0)
        );
        assert_eq!(parse_time("nowish", 0.0), None);
    }

    #[test]
    fn requests_are_blocks_between_blank_lines() {
        let script = "# range: 1h\nrate(x[5m])\n\n# just a note\n\n  up  \n# step: 1m\n\n";
        let parts: Vec<&str> = split_script(script)
            .into_iter()
            .map(|(s, e)| &script[s..e])
            .collect();
        assert_eq!(parts, ["# range: 1h\nrate(x[5m])", "up  \n# step: 1m"]);
    }

    #[test]
    fn directives_pick_instant_or_range() {
        let now = 10_000.0;
        let eval = |r: &str| parse_eval(r, now).unwrap();
        assert_eq!(eval("up"), Eval::Instant { time: None });
        // Ordinary comments, also ones with a colon or a key's name in them.
        for comment in [
            "# note: not a directive",
            "# just a note",
            "#",
            "# ranges: are fun",
            "# see https://prometheus.io/docs",
            "# range of the last hour",
        ] {
            assert_eq!(
                eval(&format!("{comment}\nup")),
                Eval::Instant { time: None },
                "{comment}"
            );
        }
        assert_eq!(
            eval("# time: 1700000000\nup"),
            Eval::Instant {
                time: Some(1_700_000_000.0)
            }
        );
        // 3600 s / 250 points, rounded up.
        assert_eq!(
            eval("# range: 1h\nrate(x[5m])"),
            Eval::Range {
                start: 6400.0,
                end: now,
                step: 15.0
            }
        );
        assert_eq!(
            eval("# range: 1h step: 30s\nup"),
            Eval::Range {
                start: 6400.0,
                end: now,
                step: 30.0
            }
        );
        // A step in seconds, as the API takes it.
        assert_eq!(
            eval("# range: 1h step: 0.5\nup"),
            Eval::Range {
                start: 6400.0,
                end: now,
                step: 0.5
            }
        );
        assert_eq!(
            eval("# start: now-2h\n# end: now-1h\nup"),
            Eval::Range {
                start: 2800.0,
                end: 6400.0,
                step: 15.0
            }
        );
        assert_eq!(
            eval("# start: 2023-11-14T22:13:20Z end: 1700003600\nup"),
            Eval::Range {
                start: 1_700_000_000.0,
                end: 1_700_003_600.0,
                step: 15.0
            }
        );
        // 300 s / 250 points, rounded up.
        assert_eq!(
            eval("# range: 5m\nup"),
            Eval::Range {
                start: 9700.0,
                end: now,
                step: 2.0
            }
        );
        // Short ranges: at least a second.
        assert_eq!(
            eval("# range: 2m\nup"),
            Eval::Range {
                start: 9880.0,
                end: now,
                step: 1.0
            }
        );
        for (bad, directive) in [
            ("# range: soon\nup", "range: soon"),
            ("# range: 1h step:\nup", "step:"),
            ("# step: 0s range: 1h\nup", "step: 0s"),
            ("# start: 10 end: 5\nup", "start: 10 end: 5"),
            ("# range: 1h foo: 2\nup", "foo:"),
            ("# range: NaN\nup", "range: NaN"),
            ("# time: inf\nup", "time: inf"),
            // A key glued to its value.
            ("# range:6h\nup", "range:6h"),
            ("#step:1m\n# range: 6h\nup", "step:1m"),
            ("# range: 6h step:1m\nup", "step:1m"),
            // `step:` and `end:` need a range.
            ("# step: 1m\nup", "step: 1m"),
            ("# end: now-1h\nup", "end: now-1h"),
            ("# time: 1700000000 step: 1m\nup", "step: 1m"),
            // `time:` is for instant queries.
            ("# time: 1700000000 range: 1h\nup", "time: 1700000000"),
            (
                "# time: 1700000000\n# start: now-1h\nup",
                "time: 1700000000",
            ),
        ] {
            assert_eq!(
                parse_eval(bad, now).unwrap_err().to_string(),
                format!("Invalid directive: {directive}"),
                "{bad}"
            );
        }
    }

    #[test]
    fn instant_vectors_are_a_row_per_series() {
        let rs = rows(
            r#"{"resultType":"vector","result":[
                {"metric":{"__name__":"up","job":"prometheus","instance":"localhost:9090"},"value":[1700000000.123,"1"]},
                {"metric":{"__name__":"up","job":"node"},"value":[1700000000.123,"NaN"]}]}"#,
            0,
        );
        assert_eq!(
            rs.columns,
            ["__name__", "instance", "job", "timestamp", "value"]
        );
        let s = |v: &str| Some(v.to_string());
        assert_eq!(
            rs.rows[0],
            [s("up"), s("localhost:9090"), s("prometheus"), s(T0), s("1")]
        );
        assert_eq!(rs.rows[1], [s("up"), None, s("node"), s(T0), s("NaN")]);
        assert!(!rs.truncated);
    }

    #[test]
    fn range_matrices_are_long_and_cut_at_max_rows() {
        let data = r#"{"resultType":"matrix","result":[
            {"metric":{"job":"a"},"values":[[1700000060,"2"],[1700000000,"1"]]},
            {"metric":{"job":"b"},"values":[[1700000000,"3"]]}]}"#;
        let rs = rows(data, 2);
        assert_eq!(rs.columns, ["job", "timestamp", "value"]);
        let s = |v: &str| Some(v.to_string());
        assert_eq!(
            rs.rows,
            [
                vec![s("a"), s("2023-11-14T22:13:20.000Z"), s("1")],
                vec![s("a"), s("2023-11-14T22:14:20.000Z"), s("2")],
            ]
        );
        assert!(rs.truncated);
        let all = rows(data, 0);
        assert_eq!((all.rows.len(), all.truncated), (3, false));
    }

    #[test]
    fn scalars_strings_and_histograms() {
        let s = |v: &str| Some(v.to_string());
        let rs = rows(
            r#"{"resultType":"scalar","result":[1700000000.123,"+Inf"]}"#,
            0,
        );
        assert_eq!(
            (rs.columns.as_slice(), rs.rows[0].as_slice()),
            (
                ["timestamp", "value"].map(String::from).as_slice(),
                [s(T0), s("+Inf")].as_slice()
            )
        );
        let rs = rows(
            r#"{"resultType":"string","result":[1700000000.123,"hello"]}"#,
            0,
        );
        assert_eq!(rs.rows[0][1], s("hello"));
        let rs = rows(
            r#"{"resultType":"vector","result":[{"metric":{},"histogram":[1700000000.123,{"count":"2","sum":"3"}]}]}"#,
            0,
        );
        assert_eq!(rs.columns, ["timestamp", "value"]);
        assert_eq!(rs.rows[0][1], s(r#"{"count":"2","sum":"3"}"#));
        assert!(result_set(&serde_json::json!({"resultType": "streams"}), 0).is_err());
    }

    #[test]
    fn auth_from_the_fields() {
        let mut c = cfg("prom", 0);
        assert_eq!(Auth::from_config(&c), Auth::None);
        c.password = " tok ".into();
        assert_eq!(Auth::from_config(&c), Auth::Bearer("tok".into()));
        c.user = "grafana".into();
        assert_eq!(
            Auth::from_config(&c),
            Auth::Basic {
                user: "grafana".into(),
                password: " tok ".into()
            }
        );
    }

    #[test]
    fn credentials_stay_off_plain_http() {
        assert!(sends_securely("https://prom.example.com"));
        assert!(sends_securely("http://127.0.0.1:9090/select/0/prometheus"));
        assert!(sends_securely("http://localhost:9090"));
        assert!(sends_securely("http://[::1]:8428"));
        assert!(sends_securely("http://127.0.0.1:9090"));
        assert!(sends_securely("http://localhost"));
        assert!(!sends_securely("http://prom.example.com:9090"));
        // A name that looks like a loopback address, and one hidden behind user info.
        assert!(!sends_securely("http://127.attacker.example:9090"));
        assert!(!sends_securely("http://localhost:80@attacker.example"));
        assert!(!sends_securely("http://127.0.0.1@attacker.example:9090"));
        assert!(!sends_securely("http://[::1"));
        // A user or password typed in the address is refused before anything is sent.
        assert!(has_credentials("http://admin:secret@prom.example.com:9090"));
        assert!(has_credentials("https://token@prom.example.com"));
        assert!(has_credentials("http://localhost:80@attacker.example"));
        assert!(!has_credentials(
            "https://prom.example.com/select/0/prometheus"
        ));
    }

    #[test]
    fn header_properties() {
        let mut c = cfg("prom", 0);
        c.set_property("header.X-Scope-OrgID", "tenant-1");
        c.set_property("tls", "true");
        let h = extra_headers(&c).unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h["x-scope-orgid"], "tenant-1");
        c.set_property("header.Bad Name", "x");
        assert!(extra_headers(&c).is_err());
    }

    #[test]
    fn credentials_are_not_header_properties() {
        for name in ["Authorization", "proxy-authorization", "COOKIE"] {
            let mut c = cfg("prom", 0);
            c.set_property(&format!("header.{name}"), "Bearer tok");
            assert_eq!(
                extra_headers(&c).unwrap_err().to_string(),
                format!(
                    "The {name} header can't be a driver property: use the \"Password or token\" field"
                )
            );
        }
    }

    /// A server on 127.0.0.1 that answers one request with `body`.
    async fn serve_once(body: &'static str) -> reqwest::Response {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let _ = socket.read(&mut request).await;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            // In two parts, so the body comes in more than one chunk.
            let (first, second) = body.split_at(body.len() / 2);
            let _ = socket.write_all(format!("{head}{first}").as_bytes()).await;
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = socket.write_all(second.as_bytes()).await;
        });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        http.get(format!("http://127.0.0.1:{port}/api/v1/query"))
            .send()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn answers_over_the_size_limit_are_refused() {
        let body = r#"{"status":"success","data":[]}"#;
        assert_eq!(
            read_body(serve_once(body).await, body.len()).await.unwrap(),
            body
        );
        // One byte over. The message names the limit in MB: 256 of them for real answers.
        let err = read_body(serve_once(body).await, body.len() - 1)
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Prometheus's answer is over 0 MB: narrow the query or its range"
        );
        assert_eq!(MAX_RESPONSE_BYTES / (1024 * 1024), 256);
    }

    #[test]
    fn api_envelopes() {
        let ok = parse_reply(
            StatusCode::OK,
            r#"{"status":"success","data":{"resultType":"scalar","result":[1,"1"]},"warnings":["partial response"],"infos":["histogram"]}"#,
        )
        .unwrap();
        assert_eq!(ok.data["resultType"], "scalar");
        assert_eq!(ok.notices, ["partial response", "histogram"]);
        let bad = parse_reply(
            StatusCode::BAD_REQUEST,
            r#"{"status":"error","errorType":"bad_data","error":"parse error at char 4"}"#,
        );
        assert_eq!(
            bad.unwrap_err().to_string(),
            "HTTP 400 Bad Request from Prometheus: bad_data: parse error at char 4"
        );
        let proxy = parse_reply(StatusCode::UNAUTHORIZED, "  Unauthorized\n");
        assert_eq!(
            proxy.unwrap_err().to_string(),
            "HTTP 401 Unauthorized from Prometheus: Unauthorized"
        );
    }

    #[test]
    fn addresses_with_path_prefixes() {
        assert_eq!(base_url(&cfg("", 9090)), "http://localhost:9090");
        assert_eq!(
            base_url(&cfg("prom.example.com", 9090)),
            "http://prom.example.com:9090"
        );
        // VictoriaMetrics cluster: vmselect's port and tenant path, trailing slash dropped.
        assert_eq!(
            base_url(&cfg(
                "http://vmselect.vm.svc:8481/select/0/prometheus/",
                9090
            )),
            "http://vmselect.vm.svc:8481/select/0/prometheus"
        );
        // Grafana's datasource proxy on 443.
        assert_eq!(
            base_url(&cfg(
                "HTTPS://grafana.example.com/api/datasources/proxy/uid/abc",
                443
            )),
            "https://grafana.example.com/api/datasources/proxy/uid/abc"
        );
        assert_eq!(base_url(&cfg("[::1]:8428", 9090)), "http://[::1]:8428");
        // A bare IPv6 address takes the port field's port.
        assert_eq!(base_url(&cfg("fd00::12", 9090)), "http://[fd00::12]:9090");
        assert_eq!(
            base_url(&cfg("http://::1/prefix", 8428)),
            "http://[::1]:8428/prefix"
        );
        let mut c = cfg("prom", 9090);
        c.set_property("tls", "true");
        assert_eq!(base_url(&c), "https://prom:9090");
    }

    #[test]
    fn metric_names_and_strings() {
        assert_eq!(
            quote_metric("node_cpu_seconds_total"),
            "node_cpu_seconds_total"
        );
        assert_eq!(quote_metric("job:up:sum"), "job:up:sum");
        assert_eq!(
            quote_metric("http.server.duration"),
            r#"{"http.server.duration"}"#
        );
        assert_eq!(quote_metric("9lives"), r#"{"9lives"}"#);
        assert_eq!(string_literal(r#"a"b\c"#), r#""a\"b\\c""#);
    }

    #[test]
    fn metrics_become_sorted_leaf_tables() {
        let tables = metric_tables(&serde_json::json!([
            "up",
            "go_goroutines",
            "up",
            "http.server.duration"
        ]));
        let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["go_goroutines", "http.server.duration", "up"]);
        assert!(
            tables
                .iter()
                .all(|t| t.kind == ObjectKind::Table && t.columns.is_empty())
        );
        assert!(metric_tables(&Value::Null).is_empty());
    }
}
