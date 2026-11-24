//! Redis / Valkey driver on top of the `redis` crate.
//!
//! The console takes one command per line, written as in `redis-cli` (`HGETALL user:1`,
//! `SET "a b" 'it''s'`…). The explorer shows each database (`db0`, `db1`…) with its keys grouped
//! by prefix: `user:*` holds `user:1`, `user:2`… and `*` holds every key. Opening a group runs
//! `SCAN 0 MATCH <pattern>`, whose rows show each key's type, TTL and a preview of its value.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use redis::aio::MultiplexedConnection;
use redis::{ConnectionAddr, IntoConnectionInfo, RedisConnectionInfo, Value};

use super::{Cell, ColumnInfo, ConnectionConfig, ObjectKind, ResultSet, SchemaInfo, TableInfo};
use crate::edit::Change;
use crate::i18n::{tr, trf};

/// Keys read to build the explorer's groups.
const INTROSPECT_KEYS: usize = 5000;
/// Elements shown in a hash, list, set or sorted set's value preview.
const PREVIEW_ITEMS: isize = 100;
/// Characters of a value preview.
const PREVIEW_CHARS: usize = 2000;
/// Characters of a string value shown whole (and editable) in the key grid.
const EDITABLE_STRING_CHARS: usize = 1_000_000;

pub struct RedisSession {
    conn: MultiplexedConnection,
    db: i64,
    closed: bool,
}

/// `db3`, `3` or empty (0) → the database number.
pub fn db_number(name: &str) -> Option<i64> {
    let n = name.trim();
    let n = n.strip_prefix("db").unwrap_or(n);
    if n.is_empty() {
        Some(0)
    } else {
        n.parse().ok().filter(|d| *d >= 0)
    }
}

fn truthy(v: Option<&str>) -> bool {
    v.is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes" | "on"))
}

impl RedisSession {
    pub async fn connect(cfg: &ConnectionConfig) -> anyhow::Result<Self> {
        let db = db_number(&cfg.database)
            .ok_or_else(|| anyhow!(tr("The Redis database must be a number (0, 1, 2…)")))?;
        let host = if cfg.host.trim().is_empty() {
            "localhost".to_string()
        } else {
            cfg.host.trim().to_string()
        };
        // TLS: the `tls` / `ssl` driver property, or a `?tls=true` URL parameter.
        let tls = truthy(cfg.property("tls"))
            || truthy(cfg.property("ssl"))
            || cfg.url_extra.to_ascii_lowercase().contains("tls=true");
        let addr = if tls {
            ConnectionAddr::TcpTls {
                host: host.clone(),
                port: cfg.port,
                insecure: truthy(cfg.property("tls_insecure")),
                tls_params: None,
            }
        } else {
            ConnectionAddr::Tcp(host.clone(), cfg.port)
        };
        let mut redis = RedisConnectionInfo::default().set_db(db);
        if !cfg.user.trim().is_empty() {
            redis = redis.set_username(cfg.user.trim());
        }
        if !cfg.password.is_empty() {
            redis = redis.set_password(&cfg.password);
        }
        let info = (host, cfg.port)
            .into_connection_info()?
            .set_addr(addr)
            .set_redis_settings(redis);
        let client = redis::Client::open(info)?;
        let conn = tokio::time::timeout(
            Duration::from_secs(10),
            client.get_multiplexed_async_connection(),
        )
        .await
        .map_err(|_| anyhow!(tr("Connection timed out")))??;
        let mut s = Self {
            conn,
            db,
            closed: false,
        };
        s.query(&["PING"]).await?;
        Ok(s)
    }

    async fn query(&mut self, args: &[&str]) -> anyhow::Result<Value> {
        let mut cmd = redis::cmd(args[0]);
        for a in &args[1..] {
            cmd.arg(*a);
        }
        self.run(cmd).await
    }

    async fn run(&mut self, cmd: redis::Cmd) -> anyhow::Result<Value> {
        match cmd.query_async::<Value>(&mut self.conn).await {
            Ok(Value::ServerError(e)) => Err(anyhow!(
                "{}",
                e.details()
                    .map_or_else(|| format!("{e:?}"), |d| format!("{} {d}", e.code()))
            )),
            Ok(v) => Ok(v),
            Err(e) => {
                if e.is_connection_dropped() || e.is_io_error() || e.is_unrecoverable_error() {
                    self.closed = true;
                }
                Err(anyhow!("{e}"))
            }
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub async fn current_database(&mut self) -> anyhow::Result<String> {
        Ok(format!("db{}", self.db))
    }

    /// The databases that have keys (`INFO keyspace`) plus the current one.
    pub async fn list_databases(&mut self) -> anyhow::Result<Vec<String>> {
        let info = text(&self.query(&["INFO", "keyspace"]).await?).unwrap_or_default();
        let mut dbs: Vec<i64> = info
            .lines()
            .filter_map(|l| l.split(':').next()?.strip_prefix("db")?.parse().ok())
            .collect();
        if !dbs.contains(&self.db) {
            dbs.push(self.db);
        }
        dbs.sort_unstable();
        Ok(dbs.into_iter().map(|d| format!("db{d}")).collect())
    }

    pub async fn use_database(&mut self, db: &str) -> anyhow::Result<()> {
        let n =
            db_number(db).ok_or_else(|| anyhow!(trf("Unknown database {db}", &[("db", &db)])))?;
        if n != self.db {
            self.query(&["SELECT", &n.to_string()]).await?;
            self.db = n;
        }
        Ok(())
    }

    /// One schema (the database) whose "tables" are key groups by prefix.
    pub async fn introspect(&mut self) -> anyhow::Result<Vec<SchemaInfo>> {
        let keys = self.scan("*", INTROSPECT_KEYS).await?;
        let mut groups: Vec<String> = Vec::new();
        for k in &keys {
            if let Some((prefix, _)) = k.split_once(':') {
                let g = format!("{prefix}:*");
                if !groups.contains(&g) {
                    groups.push(g);
                }
            }
        }
        groups.sort();
        groups.insert(0, "*".into());
        let columns = key_columns();
        let tables = groups
            .into_iter()
            .map(|name| TableInfo {
                name,
                kind: ObjectKind::Table,
                columns: columns.clone(),
            })
            .collect();
        Ok(vec![SchemaInfo {
            name: format!("db{}", self.db),
            tables,
        }])
    }

    /// Keys matching `pattern`, walking the SCAN cursor until `limit` keys.
    async fn scan(&mut self, pattern: &str, limit: usize) -> anyhow::Result<Vec<String>> {
        let mut cursor = "0".to_string();
        let mut keys = Vec::new();
        loop {
            let v = self
                .query(&["SCAN", &cursor, "MATCH", pattern, "COUNT", "1000"])
                .await?;
            let Value::Array(parts) = v else { break };
            let mut parts = parts.into_iter();
            cursor = parts
                .next()
                .as_ref()
                .and_then(text)
                .unwrap_or_else(|| "0".into());
            if let Some(Value::Array(batch)) = parts.next() {
                keys.extend(batch.iter().filter_map(text));
            }
            if cursor == "0" || keys.len() >= limit {
                break;
            }
        }
        keys.truncate(limit);
        Ok(keys)
    }

    pub async fn server_version(&mut self) -> Option<String> {
        let info = text(&self.query(&["INFO", "server"]).await.ok()?)?;
        let field = |name: &str| {
            info.lines().find_map(|l| {
                l.strip_prefix(name)?
                    .strip_prefix(':')
                    .map(|v| v.trim().to_string())
            })
        };
        match field("valkey_version") {
            Some(v) => Some(format!("Valkey {v}")),
            None => field("redis_version").map(|v| format!("Redis {v}")),
        }
    }

    /// Runs each non-empty line as a command (lines starting with `#` or `//` are comments).
    pub async fn execute(
        &mut self,
        script: &str,
        max_rows: usize,
    ) -> anyhow::Result<Vec<ResultSet>> {
        let mut out = Vec::new();
        for line in script
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("//"))
        {
            let start = Instant::now();
            let args = split_args(line)?;
            if args.is_empty() {
                continue;
            }
            let name = args[0].to_ascii_uppercase();
            let mut rs = match name.as_str() {
                // Key listings show each key with its type, TTL and value.
                "SCAN" | "KEYS" => {
                    let pattern = if name == "KEYS" {
                        args.get(1).cloned().unwrap_or_else(|| "*".into())
                    } else {
                        args.iter()
                            .position(|a| a.eq_ignore_ascii_case("MATCH"))
                            .and_then(|i| args.get(i + 1))
                            .cloned()
                            .unwrap_or_else(|| "*".into())
                    };
                    let mut keys = self.scan(&pattern, max_rows + 1).await?;
                    let truncated = keys.len() > max_rows;
                    keys.truncate(max_rows);
                    keys.sort();
                    let mut rs = self.describe_keys(&keys).await?;
                    rs.truncated = truncated;
                    rs
                }
                _ => {
                    let mut cmd = redis::cmd(&args[0]);
                    for a in &args[1..] {
                        cmd.arg(a.as_str());
                    }
                    let v = self.run(cmd).await?;
                    if name == "SELECT"
                        && let Some(n) = args.get(1).and_then(|a| a.parse().ok())
                    {
                        self.db = n;
                    }
                    value_result(&name, &args, v, max_rows)
                }
            };
            rs.statement = line.to_string();
            rs.elapsed = start.elapsed();
            out.push(rs);
        }
        Ok(out)
    }

    /// Applies data editor commands (from `change_commands`) all or nothing: their keys are
    /// WATCHed, each command's precondition is checked (the key exists; for `SET … XX` it is a
    /// string; for `SET … NX` it is new) and then they run in one MULTI / EXEC, which the server
    /// discards if a watched key changed in between.
    pub async fn apply_changes(&mut self, statements: &[String]) -> anyhow::Result<u64> {
        let mut cmds = Vec::with_capacity(statements.len());
        for stmt in statements {
            let args = split_args(stmt)?;
            let name = args
                .first()
                .map(|a| a.to_ascii_uppercase())
                .unwrap_or_default();
            if !matches!(name.as_str(), "SET" | "EXPIRE" | "PERSIST" | "DEL") || args.len() < 2 {
                anyhow::bail!(trf("Not a data editor change: {stmt}", &[("stmt", stmt)]));
            }
            cmds.push((stmt.as_str(), name, args));
        }
        if cmds.is_empty() {
            return Ok(0);
        }
        let mut keys: Vec<&str> = cmds.iter().map(|(_, _, a)| a[1].as_str()).collect();
        keys.sort_unstable();
        keys.dedup();
        let mut watch = redis::cmd("WATCH");
        for k in &keys {
            watch.arg(*k);
        }
        self.run(watch).await?;
        let result = self.apply_watched(&cmds).await;
        if result.is_err() {
            let _ = self.query(&["UNWATCH"]).await;
        }
        result
    }

    async fn apply_watched(&mut self, cmds: &[(&str, String, Vec<String>)]) -> anyhow::Result<u64> {
        let mut p = redis::pipe();
        for (_, _, args) in cmds {
            p.cmd("TYPE").arg(&args[1]);
        }
        let types: Vec<Value> = p
            .query_async(&mut self.conn)
            .await
            .map_err(|e| self.pipe_error(e))?;
        for ((stmt, name, args), ty) in cmds.iter().zip(&types) {
            // TYPE says "none" for a missing key.
            let ty = text(ty).unwrap_or_default();
            let nx = name == "SET" && args.iter().skip(3).any(|a| a.eq_ignore_ascii_case("NX"));
            match name.as_str() {
                "SET" if nx && ty != "none" => {
                    anyhow::bail!(trf("The key “{key}” already exists", &[("key", &args[1])]))
                }
                "SET" if !nx && ty != "string" && ty != "none" => {
                    anyhow::bail!(trf(
                        "Only string values can be edited; “{key}” is a {type}",
                        &[("key", &args[1]), ("type", &ty)]
                    ))
                }
                _ if !nx && ty == "none" => return Err(super::not_single_row(0, stmt)),
                _ => {}
            }
        }
        let mut p = redis::pipe();
        p.atomic();
        for (_, _, args) in cmds {
            let mut c = redis::cmd(&args[0]);
            for a in &args[1..] {
                c.arg(a.as_str());
            }
            p.add_command(c);
        }
        let reply: Value = p
            .query_async(&mut self.conn)
            .await
            .map_err(|e| self.pipe_error(e))?;
        let Value::Array(items) = reply else {
            // EXEC answers nil when a watched key changed: nothing ran.
            anyhow::bail!(tr(
                "The keys changed while saving; nothing was saved. Refresh and try again"
            ));
        };
        for ((stmt, name, _), item) in cmds.iter().zip(&items) {
            let n = match item {
                Value::Okay | Value::SimpleString(_) => 1,
                Value::Int(n) => *n,
                Value::ServerError(e) => anyhow::bail!(
                    "{}\n{stmt}",
                    e.details()
                        .map_or_else(|| format!("{e:?}"), |d| format!("{} {d}", e.code()))
                ),
                _ => 0,
            };
            // PERSIST answers 0 for a key that had no TTL.
            if n != 1 && !(name == "PERSIST" && n == 0) {
                anyhow::bail!(trf(
                    "The statement affected {n} rows (expected 1):\n{stmt}",
                    &[("n", &n), ("stmt", stmt)]
                ));
            }
        }
        Ok(cmds.len() as u64)
    }

    fn pipe_error(&mut self, e: redis::RedisError) -> anyhow::Error {
        if e.is_connection_dropped() || e.is_io_error() || e.is_unrecoverable_error() {
            self.closed = true;
        }
        anyhow!("{e}")
    }

    /// key / type / ttl / value rows for `keys`, read in pipelines.
    async fn describe_keys(&mut self, keys: &[String]) -> anyhow::Result<ResultSet> {
        let mut rows = Vec::with_capacity(keys.len());
        for chunk in keys.chunks(500) {
            let mut p = redis::pipe();
            for k in chunk {
                p.cmd("TYPE").arg(k).cmd("TTL").arg(k);
            }
            let meta: Vec<Value> = p
                .query_async(&mut self.conn)
                .await
                .map_err(|e| anyhow!("{e}"))?;
            let types: Vec<String> = meta
                .iter()
                .step_by(2)
                .map(|v| text(v).unwrap_or_default())
                .collect();
            let ttls: Vec<Option<String>> = meta
                .iter()
                .skip(1)
                .step_by(2)
                .map(|v| match v {
                    Value::Int(-1) => None,
                    Value::Int(n) => Some(n.to_string()),
                    _ => None,
                })
                .collect();
            let mut p = redis::pipe();
            for (k, ty) in chunk.iter().zip(&types) {
                match ty.as_str() {
                    "string" => p.cmd("GET").arg(k),
                    "hash" => p.cmd("HGETALL").arg(k),
                    "list" => p.cmd("LRANGE").arg(k).arg(0).arg(PREVIEW_ITEMS - 1),
                    "set" => p.cmd("SSCAN").arg(k).arg(0).arg("COUNT").arg(PREVIEW_ITEMS),
                    "zset" => p
                        .cmd("ZRANGE")
                        .arg(k)
                        .arg(0)
                        .arg(PREVIEW_ITEMS - 1)
                        .arg("WITHSCORES"),
                    "stream" => p.cmd("XLEN").arg(k),
                    _ => p.cmd("TYPE").arg(k),
                };
            }
            let values: Vec<Value> = p
                .query_async(&mut self.conn)
                .await
                .map_err(|e| anyhow!("{e}"))?;
            for (((k, ty), ttl), v) in chunk.iter().zip(types).zip(ttls).zip(values) {
                let preview = match ty.as_str() {
                    "string" => text(&v),
                    "hash" => Some(pairs_json(&v)),
                    "list" => Some(array_json(&v)),
                    "set" => match &v {
                        Value::Array(parts) => {
                            Some(array_json(parts.get(1).unwrap_or(&Value::Nil)))
                        }
                        _ => None,
                    },
                    "zset" => Some(pairs_json(&v)),
                    "stream" => text(&v).map(|n| trf("Stream entries: {n}", &[("n", &n)])),
                    _ => None,
                };
                // String values are shown whole so editing them saves exactly what the cell holds;
                // only huge ones are cut, and then their type says so and the grid refuses to edit them.
                let (ty, preview) = match (ty.as_str(), preview) {
                    ("string", Some(p)) if p.chars().count() > EDITABLE_STRING_CHARS => {
                        (tr("string (truncated)").to_string(), Some(cut(&p)))
                    }
                    ("string", p) => (ty, p),
                    (_, p) => (ty, p.map(|p| cut(&p))),
                };
                rows.push(vec![Some(k.clone()), Some(ty), ttl, preview]);
            }
        }
        Ok(ResultSet {
            columns: ["key", "type", "ttl", "value"].map(String::from).to_vec(),
            rows,
            ..Default::default()
        })
    }
}

/// Columns of a key group.
fn key_columns() -> Vec<ColumnInfo> {
    ["key", "type", "ttl", "value"]
        .iter()
        .map(|n| ColumnInfo {
            name: n.to_string(),
            data_type: "string".into(),
            nullable: *n != "key",
            primary_key: *n == "key",
        })
        .collect()
}

fn cut(s: &str) -> String {
    if s.chars().count() > PREVIEW_CHARS {
        format!("{}…", s.chars().take(PREVIEW_CHARS).collect::<String>())
    } else {
        s.to_string()
    }
}

/// A scalar reply as text (None for NULL and arrays).
fn text(v: &Value) -> Option<String> {
    match v {
        Value::Nil => None,
        Value::Int(n) => Some(n.to_string()),
        Value::BulkString(b) => Some(String::from_utf8_lossy(b).into_owned()),
        Value::SimpleString(s) => Some(s.clone()),
        Value::Okay => Some("OK".into()),
        Value::Double(d) => Some(d.to_string()),
        Value::Boolean(b) => Some(b.to_string()),
        Value::VerbatimString { text, .. } => Some(text.clone()),
        Value::BigNumber(n) => Some(format!("{n:?}")),
        _ => None,
    }
}

fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

/// Any reply as compact JSON.
fn json(v: &Value) -> String {
    match v {
        Value::Nil => "null".into(),
        Value::Int(n) => n.to_string(),
        Value::Double(d) => d.to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Array(items) | Value::Set(items) => array_json(&Value::Array(items.clone())),
        Value::Map(pairs) => {
            let fields: Vec<String> = pairs
                .iter()
                .map(|(k, v)| format!("{}:{}", json_str(&text(k).unwrap_or_default()), json(v)))
                .collect();
            format!("{{{}}}", fields.join(","))
        }
        other => json_str(&text(other).unwrap_or_default()),
    }
}

fn array_json(v: &Value) -> String {
    match v {
        Value::Array(items) | Value::Set(items) => {
            format!("[{}]", items.iter().map(json).collect::<Vec<_>>().join(","))
        }
        other => json(other),
    }
}

/// A flat `[field, value, field, value…]` reply (HGETALL, ZRANGE WITHSCORES) as a JSON object.
fn pairs_json(v: &Value) -> String {
    match v {
        Value::Array(items) => {
            let fields: Vec<String> = items
                .chunks(2)
                .map(|p| {
                    format!(
                        "{}:{}",
                        json_str(&text(&p[0]).unwrap_or_default()),
                        p.get(1).map_or("null".into(), json)
                    )
                })
                .collect();
            format!("{{{}}}", fields.join(","))
        }
        other => json(other),
    }
}

/// A command's reply as a result: pairs as field/value, lists as #/value, scalars as one value.
fn value_result(name: &str, args: &[String], v: Value, max_rows: usize) -> ResultSet {
    let cols = |c: &[&str]| c.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let cell = |v: &Value| match v {
        Value::Array(_) | Value::Map(_) | Value::Set(_) => Some(json(v)),
        other => text(other),
    };
    let with_scores = args.iter().any(|a| a.eq_ignore_ascii_case("WITHSCORES"));
    let paired = matches!(name, "HGETALL" | "CONFIG" | "HRANDFIELD" | "XINFO") || with_scores;
    let (columns, mut rows): (Vec<String>, Vec<Vec<Cell>>) = match &v {
        Value::Map(pairs) => (
            cols(&["field", "value"]),
            pairs.iter().map(|(k, v)| vec![text(k), cell(v)]).collect(),
        ),
        Value::Array(items) if paired && items.len() % 2 == 0 => {
            let header = if with_scores {
                cols(&["member", "score"])
            } else {
                cols(&["field", "value"])
            };
            (
                header,
                items
                    .chunks(2)
                    .map(|p| vec![cell(&p[0]), cell(&p[1])])
                    .collect(),
            )
        }
        Value::Array(items) | Value::Set(items) => (
            cols(&["#", "value"]),
            items
                .iter()
                .enumerate()
                .map(|(i, v)| vec![Some((i + 1).to_string()), cell(v)])
                .collect(),
        ),
        // INFO: one row per `field:value` line, with its section.
        Value::BulkString(_) | Value::VerbatimString { .. } if name == "INFO" => {
            let body = text(&v).unwrap_or_default();
            let mut section = String::new();
            let mut rows = Vec::new();
            for line in body.lines() {
                if let Some(s) = line.strip_prefix("# ") {
                    section = s.trim().to_string();
                } else if let Some((k, val)) = line.split_once(':') {
                    rows.push(vec![
                        Some(section.clone()),
                        Some(k.to_string()),
                        Some(val.trim().to_string()),
                    ]);
                }
            }
            (cols(&["section", "field", "value"]), rows)
        }
        other => (cols(&["value"]), vec![vec![text(other)]]),
    };
    let truncated = rows.len() > max_rows;
    rows.truncate(max_rows);
    ResultSet {
        columns,
        rows,
        truncated,
        ..Default::default()
    }
}

/// Splits a command line as `redis-cli` does: spaces separate arguments, `"…"` takes escapes
/// (`\n`, `\t`, `\"`, `\\`, `\xHH`) and `'…'` is literal (`\'` for a quote).
pub fn split_args(line: &str) -> anyhow::Result<Vec<String>> {
    let mut args = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let Some(&first) = chars.peek() else { break };
        let mut arg = String::new();
        match first {
            '"' => {
                chars.next();
                let mut closed = false;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => {
                            closed = true;
                            break;
                        }
                        '\\' => match chars.next() {
                            Some('n') => arg.push('\n'),
                            Some('r') => arg.push('\r'),
                            Some('t') => arg.push('\t'),
                            Some('x') => {
                                let hex: String = chars.by_ref().take(2).collect();
                                let b = u8::from_str_radix(&hex, 16)
                                    .map_err(|_| anyhow!(tr("Invalid \\x escape")))?;
                                arg.push(char::from(b));
                            }
                            Some(other) => arg.push(other),
                            None => break,
                        },
                        c => arg.push(c),
                    }
                }
                if !closed {
                    anyhow::bail!(tr("Unbalanced quotes in the command"));
                }
            }
            '\'' => {
                chars.next();
                let mut closed = false;
                while let Some(c) = chars.next() {
                    match c {
                        '\\' if chars.peek() == Some(&'\'') => {
                            chars.next();
                            arg.push('\'');
                        }
                        '\'' => {
                            closed = true;
                            break;
                        }
                        c => arg.push(c),
                    }
                }
                if !closed {
                    anyhow::bail!(tr("Unbalanced quotes in the command"));
                }
            }
            _ => {
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() {
                        break;
                    }
                    arg.push(c);
                    chars.next();
                }
            }
        }
        args.push(arg);
    }
    Ok(args)
}

// ---------------------------------------------------------------- data editing

/// An argument quoted so `split_args` reads it back as is: bare when it is a plain word,
/// otherwise in double quotes with `\"`, `\\`, `\n`, `\r`, `\t` and `\xHH` escapes.
pub fn quote_arg(s: &str) -> String {
    if !s.is_empty()
        && !s
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '\'' | '\\'))
    {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_ascii_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `EXPIRE key n`, or `PERSIST key` for an empty TTL.
fn ttl_command(key: &str, ttl: &Cell) -> Result<String, String> {
    match ttl.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        None => Ok(format!("PERSIST {}", quote_arg(key))),
        Some(t) => match t.parse::<i64>() {
            Ok(n) if n > 0 => Ok(format!("EXPIRE {} {n}", quote_arg(key))),
            _ => Err(trf(
                "“{value}” is not a valid TTL (seconds)",
                &[("value", &t)],
            )),
        },
    }
}

/// Commands that apply a change to a key group's grid (columns key / type / ttl / value).
/// Only string values can be written (the others are JSON previews); the TTL can be set or
/// removed, keys deleted and new string keys added. `types` maps each existing key to its type.
pub fn change_commands(
    change: &Change,
    types: &HashMap<String, String>,
) -> Result<Vec<String>, String> {
    let key_of = |key: &[(String, Cell)]| -> Result<String, String> {
        key.iter()
            .find(|(c, _)| c == "key")
            .and_then(|(_, v)| v.clone())
            .ok_or_else(|| tr("Key column does not exist").to_string())
    };
    match change {
        Change::Update { key, set } => {
            let k = key_of(key)?;
            let ty = types.get(&k).map_or("", String::as_str);
            let mut out = Vec::new();
            for (col, v) in set {
                match (col.as_str(), v) {
                    ("value", Some(v)) if ty == "string" => {
                        out.push(format!("SET {} {} XX KEEPTTL", quote_arg(&k), quote_arg(v)))
                    }
                    ("value", None) if ty == "string" => {
                        return Err(
                            tr("A Redis value cannot be NULL; delete the key instead").into()
                        );
                    }
                    ("value", _) => {
                        return Err(trf(
                            "Only string values can be edited; “{key}” is a {type}",
                            &[("key", &k), ("type", &ty)],
                        ));
                    }
                    ("ttl", ttl) => out.push(ttl_command(&k, ttl)?),
                    ("key", _) => return Err(tr("Keys cannot be renamed from the grid").into()),
                    ("type", _) => return Err(tr("The type of a key cannot be changed").into()),
                    _ => return Err(tr("Column does not exist").into()),
                }
            }
            Ok(out)
        }
        Change::Insert { values } => {
            let get = |c: &str| {
                values
                    .iter()
                    .find(|(n, _)| n == c)
                    .and_then(|(_, v)| v.clone())
            };
            let k = get("key")
                .filter(|k| !k.is_empty())
                .ok_or_else(|| tr("A new key needs a name").to_string())?;
            if get("type").is_some_and(|t| !t.trim().is_empty() && t.trim() != "string") {
                return Err(tr("New keys can only be strings").into());
            }
            let v = get("value").ok_or_else(|| tr("A new key needs a value").to_string())?;
            let mut cmd = format!("SET {} {} NX", quote_arg(&k), quote_arg(&v));
            if let Some(ttl) = get("ttl").filter(|t| !t.trim().is_empty()) {
                // Same validation as an edited TTL.
                ttl_command(&k, &Some(ttl.clone()))?;
                cmd.push_str(&format!(" EX {}", ttl.trim()));
            }
            Ok(vec![cmd])
        }
        Change::Delete { key } => Ok(vec![format!("DEL {}", quote_arg(&key_of(key)?))]),
    }
}

/// Commands that only read, allowed on read-only connections.
pub fn is_read_command(line: &str) -> bool {
    let Ok(args) = split_args(line) else {
        return false;
    };
    let Some(name) = args.first().map(|a| a.to_ascii_uppercase()) else {
        return true;
    };
    let sub = args
        .get(1)
        .map(|a| a.to_ascii_uppercase())
        .unwrap_or_default();
    match name.as_str() {
        "GET" | "MGET" | "STRLEN" | "GETRANGE" | "SUBSTR" | "EXISTS" | "TYPE" | "TTL" | "PTTL"
        | "EXPIRETIME" | "KEYS" | "SCAN" | "HGET" | "HMGET" | "HGETALL" | "HKEYS" | "HVALS"
        | "HLEN" | "HEXISTS" | "HSCAN" | "HSTRLEN" | "HRANDFIELD" | "LRANGE" | "LLEN"
        | "LINDEX" | "LPOS" | "SMEMBERS" | "SISMEMBER" | "SMISMEMBER" | "SCARD" | "SSCAN"
        | "SRANDMEMBER" | "SINTER" | "SUNION" | "SDIFF" | "ZRANGE" | "ZRANGEBYSCORE"
        | "ZRANGEBYLEX" | "ZREVRANGE" | "ZREVRANGEBYSCORE" | "ZSCORE" | "ZMSCORE" | "ZCARD"
        | "ZCOUNT" | "ZLEXCOUNT" | "ZRANK" | "ZREVRANK" | "ZSCAN" | "XRANGE" | "XREVRANGE"
        | "XLEN" | "XINFO" | "XPENDING" | "INFO" | "PING" | "ECHO" | "DBSIZE" | "SELECT"
        | "TIME" | "LASTSAVE" | "RANDOMKEY" | "DUMP" | "BITCOUNT" | "BITPOS" | "GETBIT"
        | "PFCOUNT" | "GEOPOS" | "GEODIST" | "GEOSEARCH" | "GEOHASH" | "JSON.GET" | "JSON.MGET"
        | "JSON.TYPE" | "JSON.STRLEN" | "JSON.ARRLEN" | "JSON.OBJKEYS" | "LOLWUT" | "ROLE"
        | "LATENCY" => true,
        "CONFIG" => sub == "GET",
        "MEMORY" => matches!(sub.as_str(), "USAGE" | "STATS" | "DOCTOR"),
        "OBJECT" => matches!(sub.as_str(), "ENCODING" | "FREQ" | "IDLETIME" | "REFCOUNT"),
        "CLIENT" => matches!(sub.as_str(), "LIST" | "INFO" | "GETNAME" | "ID"),
        "COMMAND" | "SLOWLOG" => {
            matches!(sub.as_str(), "" | "COUNT" | "INFO" | "DOCS" | "GET" | "LEN")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_like_redis_cli() {
        assert_eq!(
            split_args(r#"SET "a b" 'it\'s' x\y"#).unwrap(),
            vec!["SET", "a b", "it's", "x\\y"]
        );
        assert_eq!(
            split_args(r#"SET k "line\nnext \x41""#).unwrap(),
            vec!["SET", "k", "line\nnext A"]
        );
        assert!(split_args(r#"GET "open"#).is_err());
        assert!(split_args("   ").unwrap().is_empty());
    }

    #[test]
    fn quoted_args_split_back() {
        for s in [
            "plain",
            "",
            "a b",
            "it's",
            r#"say "hi""#,
            "back\\slash",
            "line\nnext\ttab\r",
            "bell\u{7}",
            "ñandú 雪",
            "#not-a-comment",
        ] {
            let line = format!("SET {} {}", quote_arg(s), quote_arg("v"));
            assert_eq!(
                split_args(&line).unwrap(),
                vec!["SET".to_string(), s.to_string(), "v".to_string()],
                "{line}"
            );
        }
        assert_eq!(quote_arg("user:1"), "user:1");
        assert_eq!(quote_arg("a b"), r#""a b""#);
    }

    #[test]
    fn grid_changes_become_commands() {
        let types: HashMap<String, String> = [("s", "string"), ("h", "hash")]
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .into();
        let kv = |c: &str, v: Option<&str>| (c.to_string(), v.map(str::to_string));
        let up = |k: &str, set: Vec<(String, Cell)>| {
            change_commands(
                &Change::Update {
                    key: vec![kv("key", Some(k))],
                    set,
                },
                &types,
            )
        };
        assert_eq!(
            up("s", vec![kv("value", Some("hola mundo"))]).unwrap(),
            vec![r#"SET s "hola mundo" XX KEEPTTL"#]
        );
        assert_eq!(
            up("s", vec![kv("value", Some(""))]).unwrap(),
            vec![r#"SET s "" XX KEEPTTL"#]
        );
        assert_eq!(
            up("s", vec![kv("ttl", Some("60")), kv("value", Some("x"))]).unwrap(),
            vec!["EXPIRE s 60", "SET s x XX KEEPTTL"]
        );
        assert_eq!(up("h", vec![kv("ttl", None)]).unwrap(), vec!["PERSIST h"]);
        assert_eq!(
            up("h", vec![kv("ttl", Some(" "))]).unwrap(),
            vec!["PERSIST h"]
        );
        assert!(
            up("h", vec![kv("value", Some("{}"))])
                .unwrap_err()
                .contains("hash")
        );
        assert!(up("s", vec![kv("value", None)]).is_err());
        assert!(up("s", vec![kv("ttl", Some("0"))]).is_err());
        assert!(up("s", vec![kv("ttl", Some("soon"))]).is_err());
        assert!(up("s", vec![kv("key", Some("t"))]).is_err());
        assert!(up("s", vec![kv("type", Some("hash"))]).is_err());

        let ins = |values: Vec<(String, Cell)>| change_commands(&Change::Insert { values }, &types);
        assert_eq!(
            ins(vec![kv("key", Some("new key")), kv("value", Some("1"))]).unwrap(),
            vec![r#"SET "new key" 1 NX"#]
        );
        assert_eq!(
            ins(vec![
                kv("key", Some("k")),
                kv("type", Some("string")),
                kv("ttl", Some("30")),
                kv("value", Some("v"))
            ])
            .unwrap(),
            vec!["SET k v NX EX 30"]
        );
        assert!(ins(vec![kv("value", Some("v"))]).is_err());
        assert!(ins(vec![kv("key", Some("k"))]).is_err());
        assert!(
            ins(vec![
                kv("key", Some("k")),
                kv("type", Some("list")),
                kv("value", Some("v"))
            ])
            .is_err()
        );
        assert!(
            ins(vec![
                kv("key", Some("k")),
                kv("ttl", Some("-5")),
                kv("value", Some("v"))
            ])
            .is_err()
        );

        let del = change_commands(
            &Change::Delete {
                key: vec![kv("key", Some("a \"b\""))],
            },
            &types,
        )
        .unwrap();
        assert_eq!(del, vec![r#"DEL "a \"b\"""#]);
        assert_eq!(split_args(&del[0]).unwrap(), vec!["DEL", "a \"b\""]);
    }

    #[test]
    fn read_commands() {
        assert!(is_read_command("hgetall user:1"));
        assert!(is_read_command("CONFIG GET maxmemory"));
        assert!(!is_read_command("CONFIG SET maxmemory 1"));
        assert!(!is_read_command("SET a 1"));
        assert!(!is_read_command("FLUSHALL"));
        assert!(!is_read_command("EVAL 'return 1' 0"));
    }

    #[test]
    fn database_names() {
        assert_eq!(db_number("db3"), Some(3));
        assert_eq!(db_number(" 2 "), Some(2));
        assert_eq!(db_number(""), Some(0));
        assert_eq!(db_number("x"), None);
    }

    #[test]
    fn replies_become_results() {
        let bulk = |s: &str| Value::BulkString(s.as_bytes().to_vec());
        let rs = value_result(
            "HGETALL",
            &[],
            Value::Array(vec![bulk("name"), bulk("Ana"), bulk("age"), Value::Int(3)]),
            100,
        );
        assert_eq!(rs.columns, vec!["field", "value"]);
        assert_eq!(rs.rows[1], vec![Some("age".into()), Some("3".into())]);
        let rs = value_result("LRANGE", &[], Value::Array(vec![bulk("a"), Value::Nil]), 1);
        assert_eq!(rs.columns, vec!["#", "value"]);
        assert!(rs.truncated && rs.rows.len() == 1);
        let rs = value_result(
            "INFO",
            &[],
            bulk("# Server\r\nredis_version:7.2.4\r\n\r\n# Clients\r\nconnected_clients:1\r\n"),
            100,
        );
        assert_eq!(
            rs.rows[1],
            vec![
                Some("Clients".into()),
                Some("connected_clients".into()),
                Some("1".into())
            ]
        );
        assert_eq!(
            value_result("GET", &[], Value::Nil, 10).rows,
            vec![vec![None]]
        );
        assert_eq!(
            pairs_json(&Value::Array(vec![bulk("m"), bulk("1.5")])),
            r#"{"m":"1.5"}"#
        );
    }
}
