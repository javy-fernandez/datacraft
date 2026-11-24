//! MongoDB driver on top of the official `mongodb` crate.
//!
//! The console takes `mongosh` commands: `db.users.find({age: {$gt: 30}}).sort({name: 1}).limit(10)`,
//! `db.getCollection("x").aggregate([...])`, `insertOne`, `updateMany`, `db.runCommand({...})`,
//! `show collections`, `use shop`… Arguments are relaxed JSON: unquoted keys, single quotes,
//! trailing commas, `ObjectId("…")`, `ISODate("…")`, `NumberLong(…)`, `NumberDecimal("…")` and
//! `/regex/flags`. The explorer shows collections as tables (views as views), with the fields
//! found in a sample of their documents as columns; documents show their top-level fields as
//! columns and nested documents and arrays as JSON.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use futures_util::TryStreamExt;
use mongodb::bson::{Bson, Document, doc, oid::ObjectId};
use mongodb::options::ClientOptions;
use mongodb::results::CollectionType;

use super::{Cell, ColumnInfo, ConnectionConfig, ObjectKind, ResultSet, SchemaInfo, TableInfo};
use crate::edit::Change;
use crate::i18n::{tr, trf};

/// Documents read per collection to guess its fields.
const SAMPLE: i64 = 50;
/// Collections whose fields are sampled (the rest only show `_id`).
const SAMPLED_COLLECTIONS: usize = 200;

pub struct MongoSession {
    client: mongodb::Client,
    db: String,
    closed: bool,
}

pub(crate) fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// `mongodb://user:pw@host:port/?authSource=admin…` from the dialog fields. A host ending in
/// `.mongodb.net` (Atlas) or the `srv=true` property uses `mongodb+srv://`, which has no port.
pub fn connection_string(cfg: &ConnectionConfig) -> String {
    let host = if cfg.host.trim().is_empty() {
        "localhost"
    } else {
        cfg.host.trim()
    };
    let srv = host.ends_with(".mongodb.net")
        || cfg
            .property("srv")
            .is_some_and(|v| v.eq_ignore_ascii_case("true"));
    let creds = if cfg.user.trim().is_empty() {
        String::new()
    } else {
        format!("{}:{}@", enc(cfg.user.trim()), enc(&cfg.password))
    };
    let authority = if srv || cfg.port == 0 {
        host.to_string()
    } else {
        format!("{host}:{}", cfg.port)
    };
    let scheme = if srv { "mongodb+srv" } else { "mongodb" };
    let mut params: Vec<String> = cfg
        .url_extra
        .trim()
        .trim_start_matches('?')
        .split('&')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    if !creds.is_empty()
        && !params
            .iter()
            .any(|p| p.to_ascii_lowercase().starts_with("authsource="))
    {
        params.push("authSource=admin".into());
    }
    // Through an SSH tunnel: only the server behind it (the replica set's other members aren't
    // reachable), and its certificate can't name the tunnel's 127.0.0.1.
    if cfg.tunnel_host.is_some() {
        let has = |params: &[String], name: &str| {
            params.iter().any(|p| {
                p.to_ascii_lowercase()
                    .starts_with(&format!("{}=", name.to_ascii_lowercase()))
            })
        };
        if !has(&params, "directConnection") {
            params.push("directConnection=true".into());
        }
        let tls = params
            .iter()
            .any(|p| matches!(p.to_ascii_lowercase().as_str(), "tls=true" | "ssl=true"));
        if tls && !has(&params, "tlsAllowInvalidHostnames") {
            params.push("tlsAllowInvalidHostnames=true".into());
        }
    }
    let query = if params.is_empty() {
        String::new()
    } else {
        format!("?{}", params.join("&"))
    };
    format!("{scheme}://{creds}{authority}/{query}")
}

impl MongoSession {
    pub async fn connect(cfg: &ConnectionConfig) -> anyhow::Result<Self> {
        let mut options = ClientOptions::parse(connection_string(cfg)).await?;
        options.app_name = Some("DataCraft".into());
        options.server_selection_timeout = Some(Duration::from_secs(10));
        options.connect_timeout = Some(Duration::from_secs(10));
        let client = mongodb::Client::with_options(options)?;
        let db = if cfg.database.trim().is_empty() {
            "test".to_string()
        } else {
            cfg.database.trim().to_string()
        };
        client
            .database("admin")
            .run_command(doc! { "ping": 1 })
            .await?;
        Ok(Self {
            client,
            db,
            closed: false,
        })
    }

    fn fail(&mut self, e: mongodb::error::Error) -> anyhow::Error {
        use mongodb::error::ErrorKind;
        if matches!(
            *e.kind,
            ErrorKind::Io(_)
                | ErrorKind::ServerSelection { .. }
                | ErrorKind::ConnectionPoolCleared { .. }
        ) {
            self.closed = true;
        }
        anyhow!("{e}")
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub async fn current_database(&mut self) -> anyhow::Result<String> {
        Ok(self.db.clone())
    }

    /// The server's databases; without the privilege to list them, just the current one.
    pub async fn list_databases(&mut self) -> anyhow::Result<Vec<String>> {
        let mut names = self.client.list_database_names().await.unwrap_or_default();
        if !names.contains(&self.db) {
            names.push(self.db.clone());
        }
        names.sort();
        Ok(names)
    }

    pub async fn use_database(&mut self, db: &str) -> anyhow::Result<()> {
        if !db.is_empty() {
            self.db = db.to_string();
        }
        Ok(())
    }

    pub async fn server_version(&mut self) -> Option<String> {
        let info = self
            .client
            .database("admin")
            .run_command(doc! { "buildInfo": 1 })
            .await
            .ok()?;
        info.get_str("version").ok().map(|v| format!("MongoDB {v}"))
    }

    /// Collections (and views) of the current database, with the fields of a sample of documents.
    pub async fn introspect(&mut self) -> anyhow::Result<Vec<SchemaInfo>> {
        let db = self.client.database(&self.db);
        let specs: Vec<_> = db
            .list_collections()
            .await
            .map_err(|e| self.fail(e))?
            .try_collect()
            .await
            .map_err(|e| self.fail(e))?;
        let mut tables = Vec::new();
        for (i, spec) in specs
            .into_iter()
            .filter(|s| !s.name.starts_with("system."))
            .enumerate()
        {
            // `_id` first; its type comes from the sample like the others.
            let mut fields: Vec<(String, String)> = vec![("_id".into(), "null".into())];
            if i < SAMPLED_COLLECTIONS {
                let docs: Vec<Document> = db
                    .collection::<Document>(&spec.name)
                    .find(doc! {})
                    .limit(SAMPLE)
                    .await
                    .map_err(|e| self.fail(e))?
                    .try_collect()
                    .await
                    .unwrap_or_default();
                for d in &docs {
                    for (k, v) in d {
                        match fields.iter_mut().find(|(n, _)| n == k) {
                            Some((_, ty)) if ty == "null" => *ty = type_name(v).into(),
                            Some(_) => {}
                            None => fields.push((k.clone(), type_name(v).into())),
                        }
                    }
                }
            }
            if fields[0].1 == "null" {
                fields[0].1 = "objectId".into();
            }
            let kind = if matches!(spec.collection_type, CollectionType::View) {
                ObjectKind::View
            } else {
                ObjectKind::Table
            };
            let columns = fields
                .into_iter()
                .map(|(name, data_type)| ColumnInfo {
                    primary_key: name == "_id",
                    nullable: name != "_id",
                    name,
                    data_type,
                })
                .collect();
            tables.push(TableInfo {
                name: spec.name,
                kind,
                columns,
            });
        }
        tables.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(vec![SchemaInfo {
            name: self.db.clone(),
            tables,
        }])
    }

    /// Runs ONE `mongosh` command.
    pub async fn execute(
        &mut self,
        script: &str,
        max_rows: usize,
    ) -> anyhow::Result<Vec<ResultSet>> {
        let start = Instant::now();
        let statement = script.trim().trim_end_matches(';').trim().to_string();
        if statement.is_empty() {
            return Ok(Vec::new());
        }
        let cmd = parse_command(&statement)?;
        let mut rs = self.run(cmd, max_rows).await?;
        rs.statement = statement;
        rs.elapsed = start.elapsed();
        Ok(vec![rs])
    }

    /// Applies data editor commands (`updateOne` / `insertOne` / `deleteOne` from `change_command`);
    /// each must match exactly one document. On a replica set or a sharded cluster they run in one
    /// transaction. A standalone server has no transactions: they run in order, and on a failure
    /// the earlier ones stay saved (the error says how many).
    pub async fn apply_changes(&mut self, statements: &[String]) -> anyhow::Result<u64> {
        let mut ops = Vec::with_capacity(statements.len());
        for stmt in statements {
            match parse_command(stmt.trim().trim_end_matches(';'))? {
                Command::Collection {
                    name,
                    method,
                    args,
                    chain,
                } if chain.is_empty()
                    && matches!(method.as_str(), "updateOne" | "insertOne" | "deleteOne") =>
                {
                    ops.push((stmt, name, method, args))
                }
                _ => bail!(trf("Not a data editor change: {stmt}", &[("stmt", stmt)])),
            }
        }
        let mut session = if self.supports_transactions().await {
            let mut s = self
                .client
                .start_session()
                .await
                .map_err(|e| self.fail(e))?;
            s.start_transaction().await.map_err(|e| self.fail(e))?;
            Some(s)
        } else {
            None
        };
        let db = self.client.database(&self.db);
        for (i, (stmt, name, method, args)) in ops.iter().enumerate() {
            let coll = db.collection::<Document>(name);
            let doc_arg = |n: usize| match args.get(n) {
                Some(Bson::Document(d)) => Ok(d.clone()),
                other => Err(anyhow!(trf(
                    "Expected a document, found {value}",
                    &[("value", &other.cloned().unwrap_or(Bson::Null))]
                ))),
            };
            let result = match (method.as_str(), doc_arg(0), session.as_mut()) {
                (_, Err(e), _) => Err(e),
                ("updateOne", Ok(filter), s) => match doc_arg(1) {
                    Err(e) => Err(e),
                    Ok(update) => match s {
                        Some(s) => coll.update_one(filter, update).session(s).await,
                        None => coll.update_one(filter, update).await,
                    }
                    .map(|r| r.matched_count)
                    .map_err(|e| self.fail(e)),
                },
                ("insertOne", Ok(d), s) => match s {
                    Some(s) => coll.insert_one(d).session(s).await,
                    None => coll.insert_one(d).await,
                }
                .map(|_| 1)
                .map_err(|e| self.fail(e)),
                (_, Ok(filter), s) => match s {
                    Some(s) => coll.delete_one(filter).session(s).await,
                    None => coll.delete_one(filter).await,
                }
                .map(|r| r.deleted_count)
                .map_err(|e| self.fail(e)),
            };
            let partial = session.is_none() && i > 0;
            let err = match result {
                Ok(1) => continue,
                Ok(n) if partial => anyhow!(trf(
                    "The statement affected {n} rows (expected 1):\n{stmt}",
                    &[("n", &n), ("stmt", stmt)]
                )),
                Ok(n) => super::not_single_row(n, stmt),
                Err(e) => e,
            };
            if let Some(s) = session.as_mut() {
                let _ = s.abort_transaction().await;
            } else if partial {
                let note = trf(
                    "The server does not support transactions (it is not a replica set): the {n} earlier changes were saved.",
                    &[("n", &i)],
                );
                return Err(anyhow!("{err:#}\n{note}"));
            }
            return Err(err);
        }
        if let Some(s) = session.as_mut() {
            s.commit_transaction().await.map_err(|e| self.fail(e))?;
        }
        Ok(ops.len() as u64)
    }

    /// true on a replica set member or a `mongos` router (transactions need one).
    async fn supports_transactions(&mut self) -> bool {
        let admin = self.client.database("admin");
        let hello = match admin.run_command(doc! { "hello": 1 }).await {
            Ok(h) => h,
            // Servers before 4.4 only know the legacy name.
            Err(_) => match admin.run_command(doc! { "isMaster": 1 }).await {
                Ok(h) => h,
                Err(_) => return false,
            },
        };
        hello.contains_key("setName") || hello.get_str("msg").is_ok_and(|m| m == "isdbgrid")
    }

    async fn run(&mut self, cmd: Command, max_rows: usize) -> anyhow::Result<ResultSet> {
        let db = self.client.database(&self.db);
        let limit = i64::try_from(max_rows)
            .unwrap_or(i64::MAX)
            .saturating_add(1);
        let affected = |n: u64| ResultSet {
            affected: Some(n),
            ..Default::default()
        };
        Ok(match cmd {
            Command::ShowDatabases => names_result("name", self.list_databases().await?),
            Command::ShowCollections => {
                let mut names = db.list_collection_names().await.map_err(|e| self.fail(e))?;
                names.sort();
                names_result("name", names)
            }
            Command::Use(name) => {
                self.db = name.clone();
                ResultSet {
                    columns: vec!["result".into()],
                    rows: vec![vec![Some(trf("switched to db {db}", &[("db", &name)]))]],
                    ..Default::default()
                }
            }
            Command::RunOn { admin, command } => {
                let target = if admin {
                    self.client.database("admin")
                } else {
                    db
                };
                let reply = target
                    .run_command(command)
                    .await
                    .map_err(|e| self.fail(e))?;
                documents_result(vec![reply], max_rows)
            }
            Command::Collection {
                name,
                method,
                args,
                chain,
            } => {
                let coll = db.collection::<Document>(&name);
                let arg_doc = |i: usize| -> anyhow::Result<Document> {
                    match args.get(i) {
                        None | Some(Bson::Null) => Ok(Document::new()),
                        Some(Bson::Document(d)) => Ok(d.clone()),
                        Some(other) => Err(anyhow!(trf(
                            "Expected a document, found {value}",
                            &[("value", other)]
                        ))),
                    }
                };
                match method.as_str() {
                    "find" | "findOne" => {
                        let mut find = coll.find(arg_doc(0)?);
                        if args.len() > 1 {
                            find = find.projection(arg_doc(1)?);
                        }
                        let mut want = if method == "findOne" { 1 } else { limit };
                        for (m, a) in &chain {
                            let num = || -> anyhow::Result<i64> {
                                match a.first() {
                                    Some(Bson::Int32(n)) => Ok(i64::from(*n)),
                                    Some(Bson::Int64(n)) => Ok(*n),
                                    Some(Bson::Double(n)) => Ok(*n as i64),
                                    _ => Err(anyhow!(trf(
                                        "{method}() needs a number",
                                        &[("method", m)]
                                    ))),
                                }
                            };
                            let d = || -> anyhow::Result<Document> {
                                match a.first() {
                                    Some(Bson::Document(d)) => Ok(d.clone()),
                                    _ => Err(anyhow!(trf(
                                        "{method}() needs a document",
                                        &[("method", m)]
                                    ))),
                                }
                            };
                            find = match m.as_str() {
                                "sort" => find.sort(d()?),
                                "skip" => find.skip(u64::try_from(num()?).unwrap_or(0)),
                                "projection" => find.projection(d()?),
                                "limit" => {
                                    let n = num()?;
                                    if n > 0 {
                                        want = want.min(n);
                                    }
                                    find
                                }
                                other => bail!(trf(
                                    "Unsupported method {method}",
                                    &[("method", &format!("{other}()"))]
                                )),
                            };
                        }
                        let docs =
                            take(find.limit(want).await.map_err(|e| self.fail(e))?, want).await?;
                        documents_result(docs, max_rows)
                    }
                    "aggregate" => {
                        let pipeline: Vec<Document> = match args.first() {
                            Some(Bson::Array(stages)) => stages
                                .iter()
                                .map(|s| {
                                    s.as_document().cloned().ok_or_else(|| {
                                        anyhow!(tr("Each pipeline stage must be a document"))
                                    })
                                })
                                .collect::<anyhow::Result<_>>()?,
                            None => Vec::new(),
                            _ => bail!(tr("aggregate() needs an array of stages")),
                        };
                        let cursor = coll.aggregate(pipeline).await.map_err(|e| self.fail(e))?;
                        documents_result(take(cursor, limit).await?, max_rows)
                    }
                    "countDocuments" | "count" => {
                        let n = coll
                            .count_documents(arg_doc(0)?)
                            .await
                            .map_err(|e| self.fail(e))?;
                        names_result("count", vec![n.to_string()])
                    }
                    "estimatedDocumentCount" => {
                        let n = coll
                            .estimated_document_count()
                            .await
                            .map_err(|e| self.fail(e))?;
                        names_result("count", vec![n.to_string()])
                    }
                    "distinct" => {
                        let Some(Bson::String(field)) = args.first() else {
                            bail!(tr("distinct() needs a field name"))
                        };
                        let values = coll
                            .distinct(field, arg_doc(1)?)
                            .await
                            .map_err(|e| self.fail(e))?;
                        let rows: Vec<Vec<Cell>> = values
                            .iter()
                            .take(max_rows)
                            .map(|v| vec![render(v)])
                            .collect();
                        ResultSet {
                            columns: vec![field.clone()],
                            truncated: values.len() > max_rows,
                            rows,
                            ..Default::default()
                        }
                    }
                    "getIndexes" => {
                        let indexes = db
                            .run_command(doc! { "listIndexes": &name })
                            .await
                            .map_err(|e| self.fail(e))?;
                        let batch = indexes
                            .get_document("cursor")
                            .ok()
                            .and_then(|c| c.get_array("firstBatch").ok())
                            .cloned()
                            .unwrap_or_default();
                        documents_result(
                            batch
                                .into_iter()
                                .filter_map(|b| b.as_document().cloned())
                                .collect(),
                            max_rows,
                        )
                    }
                    "insertOne" => {
                        coll.insert_one(arg_doc(0)?)
                            .await
                            .map_err(|e| self.fail(e))?;
                        affected(1)
                    }
                    "insertMany" => {
                        let Some(Bson::Array(items)) = args.first() else {
                            bail!(tr("insertMany() needs an array of documents"))
                        };
                        let docs: Vec<Document> = items
                            .iter()
                            .filter_map(|b| b.as_document().cloned())
                            .collect();
                        let n = coll
                            .insert_many(docs)
                            .await
                            .map_err(|e| self.fail(e))?
                            .inserted_ids
                            .len();
                        affected(n as u64)
                    }
                    "updateOne" | "updateMany" | "replaceOne" => {
                        let filter = arg_doc(0)?;
                        let r = match (method.as_str(), args.get(1)) {
                            ("replaceOne", _) => coll.replace_one(filter, arg_doc(1)?).await,
                            ("updateOne", Some(Bson::Array(p))) => {
                                coll.update_one(filter, pipeline_of(p)?).await
                            }
                            ("updateMany", Some(Bson::Array(p))) => {
                                coll.update_many(filter, pipeline_of(p)?).await
                            }
                            ("updateOne", _) => coll.update_one(filter, arg_doc(1)?).await,
                            _ => coll.update_many(filter, arg_doc(1)?).await,
                        }
                        .map_err(|e| self.fail(e))?;
                        affected(r.modified_count)
                    }
                    "deleteOne" => affected(
                        coll.delete_one(arg_doc(0)?)
                            .await
                            .map_err(|e| self.fail(e))?
                            .deleted_count,
                    ),
                    "deleteMany" => affected(
                        coll.delete_many(arg_doc(0)?)
                            .await
                            .map_err(|e| self.fail(e))?
                            .deleted_count,
                    ),
                    "drop" => {
                        coll.drop().await.map_err(|e| self.fail(e))?;
                        affected(0)
                    }
                    "createIndex" => {
                        let model = mongodb::IndexModel::builder().keys(arg_doc(0)?).build();
                        let r = coll.create_index(model).await.map_err(|e| self.fail(e))?;
                        names_result("index", vec![r.index_name])
                    }
                    other => bail!(trf(
                        "Unsupported method {method}",
                        &[("method", &format!("{other}()"))]
                    )),
                }
            }
        })
    }
}

fn pipeline_of(stages: &[Bson]) -> anyhow::Result<Vec<Document>> {
    stages
        .iter()
        .map(|s| {
            s.as_document()
                .cloned()
                .ok_or_else(|| anyhow!(tr("Each pipeline stage must be a document")))
        })
        .collect()
}

/// Up to `n` documents of a cursor.
async fn take(mut cursor: mongodb::Cursor<Document>, n: i64) -> anyhow::Result<Vec<Document>> {
    let mut docs = Vec::new();
    while (docs.len() as i64) < n {
        match cursor.try_next().await? {
            Some(d) => docs.push(d),
            None => break,
        }
    }
    Ok(docs)
}

fn names_result(column: &str, names: Vec<String>) -> ResultSet {
    ResultSet {
        columns: vec![column.into()],
        rows: names.into_iter().map(|n| vec![Some(n)]).collect(),
        ..Default::default()
    }
}

/// Documents as rows: top-level fields as columns (`_id` first, then in order of appearance).
fn documents_result(docs: Vec<Document>, max_rows: usize) -> ResultSet {
    let truncated = docs.len() > max_rows;
    let docs = &docs[..docs.len().min(max_rows)];
    let mut columns: Vec<String> = Vec::new();
    if docs.iter().any(|d| d.contains_key("_id")) {
        columns.push("_id".into());
    }
    for d in docs {
        for k in d.keys() {
            if !columns.contains(k) {
                columns.push(k.clone());
            }
        }
    }
    let rows = docs
        .iter()
        .map(|d| columns.iter().map(|c| d.get(c).and_then(render)).collect())
        .collect();
    ResultSet {
        columns,
        rows,
        truncated,
        ..Default::default()
    }
}

fn type_name(v: &Bson) -> &'static str {
    match v {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Document(_) => "object",
        Bson::Array(_) => "array",
        Bson::Binary(_) => "binData",
        Bson::ObjectId(_) => "objectId",
        Bson::Boolean(_) => "bool",
        Bson::DateTime(_) => "date",
        Bson::Null => "null",
        Bson::RegularExpression(_) => "regex",
        Bson::Int32(_) => "int",
        Bson::Timestamp(_) => "timestamp",
        Bson::Int64(_) => "long",
        Bson::Decimal128(_) => "decimal",
        _ => "mixed",
    }
}

/// A value as the grid shows it: scalars as text, documents and arrays as compact JSON.
pub fn render(v: &Bson) -> Cell {
    Some(match v {
        Bson::Null | Bson::Undefined => return None,
        Bson::String(s) => s.clone(),
        Bson::Int32(n) => n.to_string(),
        Bson::Int64(n) => n.to_string(),
        Bson::Double(n) => n.to_string(),
        Bson::Boolean(b) => b.to_string(),
        Bson::ObjectId(o) => o.to_hex(),
        Bson::DateTime(d) => d.try_to_rfc3339_string().unwrap_or_else(|_| d.to_string()),
        Bson::Decimal128(d) => d.to_string(),
        Bson::Binary(b) => {
            let hex: String = b
                .bytes
                .iter()
                .take(64)
                .map(|x| format!("{x:02X}"))
                .collect();
            format!("0x{hex}{}", if b.bytes.len() > 64 { "…" } else { "" })
        }
        other => serde_json::to_string(&other.clone().into_relaxed_extjson()).unwrap_or_default(),
    })
}

/// A parsed console command.
#[derive(Debug, PartialEq)]
pub enum Command {
    ShowDatabases,
    ShowCollections,
    Use(String),
    /// `db.runCommand(…)` / `db.adminCommand(…)`.
    RunOn {
        admin: bool,
        command: Document,
    },
    Collection {
        name: String,
        method: String,
        args: Vec<Bson>,
        chain: Vec<(String, Vec<Bson>)>,
    },
}

/// Methods that only read, allowed on read-only connections.
pub fn is_read_command(statement: &str) -> bool {
    match parse_command(statement.trim().trim_end_matches(';')) {
        Ok(Command::ShowDatabases | Command::ShowCollections | Command::Use(_)) => true,
        Ok(Command::RunOn { command, .. }) => command.keys().next().is_some_and(|k| {
            matches!(
                k.as_str(),
                "ping"
                    | "buildInfo"
                    | "serverStatus"
                    | "dbStats"
                    | "collStats"
                    | "listCollections"
                    | "listDatabases"
                    | "hello"
                    | "isMaster"
                    | "count"
                    | "listIndexes"
                    | "connectionStatus"
                    | "hostInfo"
                    | "getParameter"
            )
        }),
        Ok(Command::Collection { method, args, .. }) => match method.as_str() {
            "find"
            | "findOne"
            | "countDocuments"
            | "count"
            | "estimatedDocumentCount"
            | "distinct"
            | "getIndexes" => true,
            // $out and $merge write the pipeline's result to a collection.
            "aggregate" => !args.iter().any(|a| contains_key(a, &["$out", "$merge"])),
            _ => false,
        },
        Err(_) => false,
    }
}

fn contains_key(v: &Bson, keys: &[&str]) -> bool {
    match v {
        Bson::Document(d) => d
            .iter()
            .any(|(k, v)| keys.contains(&k.as_str()) || contains_key(v, keys)),
        Bson::Array(a) => a.iter().any(|v| contains_key(v, keys)),
        _ => false,
    }
}

/// `db.x.find(...)`, `db.getCollection("x").find(...)`, `db["x"].find(...)`, `show dbs`, `use x`…
pub fn parse_command(text: &str) -> anyhow::Result<Command> {
    let t = text.trim();
    let words: Vec<&str> = t.split_whitespace().collect();
    match words.as_slice() {
        ["show", "dbs" | "databases"] => return Ok(Command::ShowDatabases),
        ["show", "collections" | "tables"] => return Ok(Command::ShowCollections),
        ["use", name] => return Ok(Command::Use(name.trim_matches(['"', '\'']).to_string())),
        _ => {}
    }
    let mut p = Parser {
        s: t.as_bytes(),
        src: t,
        i: 0,
    };
    p.expect_word("db")?;
    let name = if p.eat(b'[') {
        let Bson::String(n) = p.value()? else {
            bail!(tr("Expected a collection name"))
        };
        p.expect(b']')?;
        n
    } else {
        p.expect(b'.')?;
        let first = p.ident()?;
        match first.as_str() {
            "getCollection" => {
                let args = p.args()?;
                match args.first() {
                    Some(Bson::String(n)) => n.clone(),
                    _ => bail!(tr("getCollection() needs a collection name")),
                }
            }
            "runCommand" | "adminCommand" => {
                let args = p.args()?;
                p.end()?;
                let Some(Bson::Document(command)) = args.into_iter().next() else {
                    bail!(tr("runCommand() needs a document"))
                };
                return Ok(Command::RunOn {
                    admin: first == "adminCommand",
                    command,
                });
            }
            "getCollectionNames" => {
                p.args()?;
                p.end()?;
                return Ok(Command::ShowCollections);
            }
            _ => first,
        }
    };
    p.expect(b'.')?;
    let method = p.ident()?;
    let args = p.args()?;
    let mut chain = Vec::new();
    while p.eat(b'.') {
        let m = p.ident()?;
        chain.push((m, p.args()?));
    }
    p.end()?;
    Ok(Command::Collection {
        name,
        method,
        args,
        chain,
    })
}

/// Relaxed JSON / `mongosh` argument parser.
struct Parser<'a> {
    s: &'a [u8],
    src: &'a str,
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        loop {
            while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
                self.i += 1;
            }
            if self.src[self.i..].starts_with("//") {
                self.i = self.src[self.i..]
                    .find('\n')
                    .map_or(self.s.len(), |n| self.i + n);
            } else {
                break;
            }
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.ws();
        self.s.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn err(&self, what: &str) -> anyhow::Error {
        let near: String = self.src[self.i.min(self.src.len())..]
            .chars()
            .take(20)
            .collect();
        anyhow!(trf(
            "Syntax error: expected {what} near “{near}”",
            &[("what", &what), ("near", &near)]
        ))
    }

    fn expect(&mut self, c: u8) -> anyhow::Result<()> {
        if self.eat(c) {
            Ok(())
        } else {
            Err(self.err(&(c as char).to_string()))
        }
    }

    fn end(&mut self) -> anyhow::Result<()> {
        if self.peek().is_none() {
            Ok(())
        } else {
            Err(self.err(tr("the end of the command")))
        }
    }

    fn ident(&mut self) -> anyhow::Result<String> {
        self.ws();
        let start = self.i;
        while self.i < self.s.len()
            && (self.s[self.i].is_ascii_alphanumeric() || matches!(self.s[self.i], b'_' | b'$'))
        {
            self.i += 1;
        }
        if start == self.i {
            return Err(self.err(tr("a name")));
        }
        Ok(self.src[start..self.i].to_string())
    }

    fn expect_word(&mut self, w: &str) -> anyhow::Result<()> {
        let got = self.ident()?;
        if got == w {
            Ok(())
        } else {
            Err(anyhow!(tr("Commands start with db., show or use")))
        }
    }

    /// `( value, value… )`
    fn args(&mut self) -> anyhow::Result<Vec<Bson>> {
        self.expect(b'(')?;
        let mut out = Vec::new();
        while !self.eat(b')') {
            out.push(self.value()?);
            if !self.eat(b',') {
                self.expect(b')')?;
                break;
            }
        }
        Ok(out)
    }

    fn string(&mut self, quote: u8) -> anyhow::Result<String> {
        self.i += 1;
        let mut out = String::new();
        let mut chars = self.src[self.i..].char_indices();
        while let Some((n, c)) = chars.next() {
            match c {
                c if c as u32 == quote as u32 => {
                    self.i += n + 1;
                    return Ok(out);
                }
                '\\' => match chars.next() {
                    Some((_, 'n')) => out.push('\n'),
                    Some((_, 't')) => out.push('\t'),
                    Some((_, 'r')) => out.push('\r'),
                    Some((_, 'u')) => {
                        let hex: String = chars.by_ref().take(4).map(|(_, c)| c).collect();
                        out.push(
                            u32::from_str_radix(&hex, 16)
                                .ok()
                                .and_then(char::from_u32)
                                .unwrap_or('\u{fffd}'),
                        );
                    }
                    Some((_, other)) => out.push(other),
                    None => break,
                },
                c => out.push(c),
            }
        }
        Err(anyhow!(tr("Unterminated string")))
    }

    fn value(&mut self) -> anyhow::Result<Bson> {
        match self.peek() {
            Some(b'{') => {
                self.i += 1;
                let mut d = Document::new();
                while !self.eat(b'}') {
                    let key = match self.peek() {
                        Some(q @ (b'"' | b'\'')) => self.string(q)?,
                        _ => self.ident()?,
                    };
                    self.expect(b':')?;
                    let v = self.value()?;
                    d.insert(key, v);
                    if !self.eat(b',') {
                        self.expect(b'}')?;
                        break;
                    }
                }
                Ok(Bson::Document(d))
            }
            Some(b'[') => {
                self.i += 1;
                let mut a = Vec::new();
                while !self.eat(b']') {
                    a.push(self.value()?);
                    if !self.eat(b',') {
                        self.expect(b']')?;
                        break;
                    }
                }
                Ok(Bson::Array(a))
            }
            Some(q @ (b'"' | b'\'')) => Ok(Bson::String(self.string(q)?)),
            Some(b'/') => {
                self.i += 1;
                let start = self.i;
                while self.i < self.s.len() && self.s[self.i] != b'/' {
                    if self.s[self.i] == b'\\' {
                        self.i += 1;
                    }
                    self.i += 1;
                }
                let pattern = self.src[start..self.i.min(self.s.len())].to_string();
                self.expect(b'/')?;
                let fstart = self.i;
                while self.i < self.s.len() && self.s[self.i].is_ascii_alphabetic() {
                    self.i += 1;
                }
                let mut flags: Vec<char> = self.src[fstart..self.i].chars().collect();
                flags.sort_unstable();
                Ok(Bson::RegularExpression(mongodb::bson::Regex {
                    pattern,
                    options: flags.into_iter().collect(),
                }))
            }
            Some(c) if c == b'-' || c == b'+' || c == b'.' || c.is_ascii_digit() => {
                let start = self.i;
                self.i += 1;
                while self.i < self.s.len()
                    && (self.s[self.i].is_ascii_digit()
                        || matches!(self.s[self.i], b'.' | b'e' | b'E' | b'-' | b'+'))
                {
                    self.i += 1;
                }
                let n = &self.src[start..self.i];
                if let Ok(v) = n.parse::<i32>() {
                    Ok(Bson::Int32(v))
                } else if let Ok(v) = n.parse::<i64>() {
                    Ok(Bson::Int64(v))
                } else {
                    n.parse::<f64>()
                        .map(Bson::Double)
                        .map_err(|_| self.err(tr("a number")))
                }
            }
            Some(_) => {
                let mut word = self.ident()?;
                if word == "new" {
                    word = self.ident()?;
                }
                match word.as_str() {
                    "true" => Ok(Bson::Boolean(true)),
                    "false" => Ok(Bson::Boolean(false)),
                    "null" | "undefined" => Ok(Bson::Null),
                    _ => {
                        let args = self.args()?;
                        let text = |a: Option<&Bson>| match a {
                            Some(Bson::String(s)) => Some(s.clone()),
                            Some(Bson::Int32(n)) => Some(n.to_string()),
                            Some(Bson::Int64(n)) => Some(n.to_string()),
                            Some(Bson::Double(n)) => Some(n.to_string()),
                            _ => None,
                        };
                        let arg = text(args.first());
                        match word.as_str() {
                            "ObjectId" => match arg {
                                Some(hex) => Ok(Bson::ObjectId(
                                    ObjectId::parse_str(&hex).map_err(|e| anyhow!("{e}"))?,
                                )),
                                None => Ok(Bson::ObjectId(ObjectId::new())),
                            },
                            "ISODate" | "Date" => match arg {
                                Some(s) => {
                                    Ok(Bson::DateTime(parse_date(&s).map_err(|e| anyhow!("{e}"))?))
                                }
                                None => Ok(Bson::DateTime(mongodb::bson::DateTime::now())),
                            },
                            "NumberLong" => Ok(Bson::Int64(
                                arg.and_then(|s| s.parse().ok())
                                    .ok_or_else(|| self.err(tr("a number")))?,
                            )),
                            "NumberInt" => Ok(Bson::Int32(
                                arg.and_then(|s| s.parse().ok())
                                    .ok_or_else(|| self.err(tr("a number")))?,
                            )),
                            "NumberDecimal" => Ok(Bson::Decimal128(
                                arg.and_then(|s| s.parse().ok())
                                    .ok_or_else(|| self.err(tr("a number")))?,
                            )),
                            other => Err(anyhow!(trf(
                                "Unknown function {name}",
                                &[("name", &format!("{other}()"))]
                            ))),
                        }
                    }
                }
            }
            None => Err(self.err(tr("a value"))),
        }
    }
}

/// Where a `mongosh` script splits: at `;` and at line ends outside brackets and strings, unless
/// the next line goes on with `.method()`. Returns byte ranges.
pub fn split_script(script: &str) -> Vec<(usize, usize)> {
    let b = script.as_bytes();
    let mut out = Vec::new();
    let (mut depth, mut start, mut i) = (0i32, None::<usize>, 0usize);
    let push = |s: Option<usize>, e: usize, out: &mut Vec<(usize, usize)>| {
        if let Some(s) = s {
            let text = script[s..e].trim_end();
            if !text.is_empty() {
                out.push((s, s + text.len()));
            }
        }
    };
    while i < b.len() {
        let c = b[i];
        if start.is_none() && !c.is_ascii_whitespace() {
            if script[i..].starts_with("//") {
                i = script[i..].find('\n').map_or(b.len(), |n| i + n);
                continue;
            }
            start = Some(i);
        }
        match c {
            b'"' | b'\'' => {
                i += 1;
                while i < b.len() && b[i] != c {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b';' if depth <= 0 => {
                push(start.take(), i, &mut out);
                depth = 0;
            }
            b'\n' if depth <= 0 && start.is_some() => {
                let rest = script[i + 1..].trim_start();
                if !rest.starts_with('.') {
                    push(start.take(), i, &mut out);
                    depth = 0;
                }
            }
            _ => {}
        }
        i += 1;
    }
    push(start, b.len(), &mut out);
    out
}

impl MongoSession {
    /// "Copy Table To…" target: inserts the documents with one unordered `insertMany`, creating
    /// the collection if needed. Documents whose `_id` is already there (duplicate key) are
    /// skipped instead of failing the copy; any other error fails it. Returns (inserted, skipped).
    pub async fn insert_skipping_duplicates(
        &mut self,
        collection: &str,
        docs: Vec<Document>,
    ) -> anyhow::Result<(u64, u64)> {
        use mongodb::error::ErrorKind;
        let n = docs.len() as u64;
        if n == 0 {
            return Ok((0, 0));
        }
        let coll = self
            .client
            .database(&self.db)
            .collection::<Document>(collection);
        match coll.insert_many(docs).ordered(false).await {
            Ok(r) => Ok((r.inserted_ids.len() as u64, 0)),
            Err(e) => {
                if let ErrorKind::InsertMany(ref im) = *e.kind
                    && im.write_concern_error.is_none()
                    && let Some(errors) = &im.write_errors
                    && errors.iter().all(|w| w.code == DUPLICATE_KEY)
                {
                    let skipped = errors.len() as u64;
                    return Ok((n.saturating_sub(skipped), skipped));
                }
                Err(self.fail(e))
            }
        }
    }
}

/// Server error code of a write that breaks a unique index (`_id` included).
const DUPLICATE_KEY: i32 = 11000;

/// `ISODate("…")` text: RFC 3339, or just a date (`2026-01-02`, midnight UTC).
fn parse_date(s: &str) -> Result<mongodb::bson::DateTime, mongodb::bson::datetime::Error> {
    let s = if s.len() == 10 {
        format!("{s}T00:00:00Z")
    } else {
        s.to_string()
    };
    mongodb::bson::DateTime::parse_rfc3339_str(&s)
}

// ---------------------------------------------------------------- data editing

/// A string as the console parser reads it (double quotes; `\n`, `\t`, `\r` and `\uXXXX` escapes).
fn shell_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A BSON value in `mongosh` syntax, which `parse_command` reads back with the same type.
fn to_shell(v: &Bson) -> Result<String, String> {
    Ok(match v {
        Bson::Null | Bson::Undefined => "null".into(),
        Bson::String(s) => shell_str(s),
        Bson::Boolean(b) => b.to_string(),
        Bson::Int32(n) => n.to_string(),
        Bson::Int64(n) => format!("NumberLong(\"{n}\")"),
        // `{:?}` keeps the decimal point (`1.0`), so the parser reads a double back.
        Bson::Double(n) if n.is_finite() => format!("{n:?}"),
        Bson::Decimal128(d) => format!("NumberDecimal({})", shell_str(&d.to_string())),
        Bson::ObjectId(o) => format!("ObjectId(\"{}\")", o.to_hex()),
        Bson::DateTime(d) => format!(
            "ISODate({})",
            shell_str(&d.try_to_rfc3339_string().map_err(|e| e.to_string())?)
        ),
        Bson::RegularExpression(r) if !r.pattern.contains('/') => {
            format!("/{}/{}", r.pattern, r.options)
        }
        Bson::Document(d) => {
            let fields = d
                .iter()
                .map(|(k, v)| Ok(format!("{}: {}", shell_str(k), to_shell(v)?)))
                .collect::<Result<Vec<_>, String>>()?;
            format!("{{{}}}", fields.join(", "))
        }
        Bson::Array(a) => format!(
            "[{}]",
            a.iter()
                .map(to_shell)
                .collect::<Result<Vec<_>, String>>()?
                .join(", ")
        ),
        other => {
            return Err(trf(
                "Values of type {type} cannot be written from the grid",
                &[("type", &type_name(other))],
            ));
        }
    })
}

/// A grid cell (text) as a `mongosh` value of the field's sampled type (`ColumnInfo::data_type`
/// from `introspect`). NULL is `null`; strings and unknown types are written as strings.
pub fn shell_value(value: &Cell, data_type: &str) -> Result<String, String> {
    let Some(v) = value else {
        return Ok("null".into());
    };
    let bad = || {
        trf(
            "“{value}” is not a valid {type} value",
            &[("value", v), ("type", &data_type)],
        )
    };
    let t = v.trim();
    Ok(match data_type {
        "objectId" => format!(
            "ObjectId(\"{}\")",
            ObjectId::parse_str(t).map_err(|_| bad())?.to_hex()
        ),
        "int" => match t.parse::<i32>() {
            Ok(n) => n.to_string(),
            Err(_) => format!("NumberLong(\"{}\")", t.parse::<i64>().map_err(|_| bad())?),
        },
        "long" => format!("NumberLong(\"{}\")", t.parse::<i64>().map_err(|_| bad())?),
        "double" => format!(
            "{:?}",
            t.parse::<f64>()
                .ok()
                .filter(|n| n.is_finite())
                .ok_or_else(bad)?
        ),
        "decimal" => format!(
            "NumberDecimal({})",
            shell_str(
                &t.parse::<mongodb::bson::Decimal128>()
                    .map_err(|_| bad())?
                    .to_string()
            )
        ),
        "bool" => match t.to_ascii_lowercase().as_str() {
            "true" => "true".into(),
            "false" => "false".into(),
            _ => return Err(bad()),
        },
        "date" => {
            parse_date(t).map_err(|_| bad())?;
            format!("ISODate({})", shell_str(t))
        }
        // The grid shows them as relaxed extended JSON (`{"$oid": …}`, `{"$date": …}`), read back here.
        "object" | "array" => {
            let json: serde_json::Value = serde_json::from_str(t).map_err(|_| bad())?;
            to_shell(&Bson::try_from(json).map_err(|e| format!("{}: {e}", bad()))?)?
        }
        _ => shell_str(v),
    })
}

/// The `mongosh` command that applies a data editor change to `collection`: `updateOne` with
/// `$set`, `insertOne` or `deleteOne`, by `_id`. `types` maps each field to its sampled type.
pub fn change_command(
    collection: &str,
    change: &Change,
    types: &HashMap<String, String>,
) -> Result<String, String> {
    let ty = |c: &str| types.get(c).map_or("mixed", String::as_str);
    let fields = |pairs: &[(String, Cell)]| -> Result<String, String> {
        let parts = pairs
            .iter()
            .map(|(c, v)| {
                Ok(format!(
                    "{}: {}",
                    shell_str(c),
                    shell_value(v, ty(c)).map_err(|e| format!("{c}: {e}"))?
                ))
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(format!("{{{}}}", parts.join(", ")))
    };
    // The key falls back to a string when it doesn't fit the sampled type (a field may mix types).
    let by_id = |key: &[(String, Cell)]| -> Result<String, String> {
        let (_, id) = key
            .iter()
            .find(|(c, _)| c == "_id")
            .ok_or_else(|| tr("The documents have no _id to identify them").to_string())?;
        let v = shell_value(id, ty("_id")).or_else(|_| shell_value(id, "string"))?;
        Ok(format!("{{\"_id\": {v}}}"))
    };
    let coll = format!("db.getCollection({})", shell_str(collection));
    match change {
        Change::Update { key, set } => {
            if set.iter().any(|(c, _)| c == "_id") {
                return Err(tr("The _id of an existing document cannot be changed").into());
            }
            Ok(format!(
                "{coll}.updateOne({}, {{\"$set\": {}}})",
                by_id(key)?,
                fields(set)?
            ))
        }
        Change::Insert { values } => Ok(format!("{coll}.insertOne({})", fields(values)?)),
        Change::Delete { key } => Ok(format!("{coll}.deleteOne({})", by_id(key)?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mongosh_commands() {
        let c = parse_command(r#"db.users.find({age: {$gt: 30}, name: 'Ana', tags: ["a", 'b',],}, {_id: 0}).sort({name: -1}).limit(5)"#).unwrap();
        let Command::Collection {
            name,
            method,
            args,
            chain,
        } = c
        else {
            panic!()
        };
        assert_eq!((name.as_str(), method.as_str()), ("users", "find"));
        assert_eq!(
            args[0],
            Bson::Document(doc! { "age": { "$gt": 30 }, "name": "Ana", "tags": ["a", "b"] })
        );
        assert_eq!(
            chain,
            vec![
                (
                    "sort".to_string(),
                    vec![Bson::Document(doc! { "name": -1 })]
                ),
                ("limit".to_string(), vec![Bson::Int32(5)])
            ]
        );

        let c = parse_command(r#"db.getCollection("my coll").insertOne({_id: ObjectId("64b7f1f1f1f1f1f1f1f1f1f1"), at: ISODate("2026-01-02T03:04:05Z"), n: NumberLong("9007199254740993"), d: NumberDecimal("1.10"), re: /^ab/i, x: 1.5e3, z: null})"#).unwrap();
        let Command::Collection { name, args, .. } = c else {
            panic!()
        };
        assert_eq!(name, "my coll");
        let d = args[0].as_document().unwrap();
        assert_eq!(
            d.get_object_id("_id").unwrap().to_hex(),
            "64b7f1f1f1f1f1f1f1f1f1f1"
        );
        assert_eq!(d.get_i64("n").unwrap(), 9007199254740993);
        assert_eq!(d.get_f64("x").unwrap(), 1500.0);
        assert!(
            matches!(d.get("re"), Some(Bson::RegularExpression(r)) if r.pattern == "^ab" && r.options == "i")
        );
        assert_eq!(
            render(d.get("at").unwrap()).unwrap(),
            "2026-01-02T03:04:05Z"
        );

        assert_eq!(
            parse_command("db['x'].countDocuments()").unwrap(),
            Command::Collection {
                name: "x".into(),
                method: "countDocuments".into(),
                args: vec![],
                chain: vec![]
            }
        );
        assert_eq!(parse_command("show  dbs").unwrap(), Command::ShowDatabases);
        assert_eq!(
            parse_command("use shop").unwrap(),
            Command::Use("shop".into())
        );
        assert!(matches!(
            parse_command("db.adminCommand({ping: 1})").unwrap(),
            Command::RunOn { admin: true, .. }
        ));
        assert!(parse_command("db.x.find({a: })").is_err());
        assert!(parse_command("select * from x").is_err());
        assert!(parse_command("db.x.find() junk").is_err());
    }

    #[test]
    fn grid_values_keep_their_type() {
        let v = |s: &str, ty: &str| shell_value(&Some(s.to_string()), ty);
        assert_eq!(
            v("64B7F1F1F1F1F1F1F1F1F1F1", "objectId").unwrap(),
            r#"ObjectId("64b7f1f1f1f1f1f1f1f1f1f1")"#
        );
        assert!(v("xyz", "objectId").is_err());
        assert_eq!(v(" 42 ", "int").unwrap(), "42");
        assert_eq!(
            v("9007199254740993", "int").unwrap(),
            r#"NumberLong("9007199254740993")"#
        );
        assert_eq!(v("7", "long").unwrap(), r#"NumberLong("7")"#);
        assert_eq!(v("3", "double").unwrap(), "3.0");
        assert_eq!(v("2.5", "double").unwrap(), "2.5");
        assert!(
            v("abc", "int").is_err() && v("NaN", "double").is_err() && v("1.5", "long").is_err()
        );
        assert_eq!(v("1.10", "decimal").unwrap(), r#"NumberDecimal("1.10")"#);
        assert_eq!(v("TRUE", "bool").unwrap(), "true");
        assert!(v("yes", "bool").is_err());
        assert_eq!(
            v("2026-01-02T03:04:05Z", "date").unwrap(),
            r#"ISODate("2026-01-02T03:04:05Z")"#
        );
        assert_eq!(v("2026-01-02", "date").unwrap(), r#"ISODate("2026-01-02")"#);
        assert!(v("yesterday", "date").is_err());
        assert_eq!(
            v(r#"say "hi" \ ok"#, "string").unwrap(),
            r#""say \"hi\" \\ ok""#
        );
        assert_eq!(v("", "string").unwrap(), r#""""#);
        assert_eq!(v("42", "mixed").unwrap(), r#""42""#);
        assert_eq!(shell_value(&None, "int").unwrap(), "null");
        // Objects and arrays come back from the grid's relaxed extended JSON.
        assert_eq!(
            v(r#"{"a":1,"b":[true,null],"id":{"$oid":"64b7f1f1f1f1f1f1f1f1f1f1"},"at":{"$date":"2026-01-02T00:00:00Z"},"x":1.0}"#, "object").unwrap(),
            r#"{"a": 1, "b": [true, null], "id": ObjectId("64b7f1f1f1f1f1f1f1f1f1f1"), "at": ISODate("2026-01-02T00:00:00Z"), "x": 1.0}"#
        );
        assert_eq!(v("[]", "array").unwrap(), "[]");
        assert!(v("{bad", "object").is_err());
        assert!(
            v(
                r#"{"b":{"$binary":{"base64":"AA==","subType":"00"}}}"#,
                "object"
            )
            .is_err()
        );
    }

    #[test]
    fn grid_changes_become_commands_the_parser_reads() {
        let types: HashMap<String, String> = [
            ("_id", "objectId"),
            ("n", "int"),
            ("name", "string"),
            ("tags", "array"),
            ("at", "date"),
        ]
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .into();
        let kv = |c: &str, v: Option<&str>| (c.to_string(), v.map(str::to_string));
        let id = kv("_id", Some("64b7f1f1f1f1f1f1f1f1f1f1"));
        let oid = ObjectId::parse_str("64b7f1f1f1f1f1f1f1f1f1f1").unwrap();

        let up = Change::Update {
            key: vec![id.clone()],
            set: vec![
                kv("n", Some("5")),
                kv("name", None),
                kv("tags", Some(r#"["a",2]"#)),
                kv("note", Some("")),
            ],
        };
        let cmd = change_command("my \"coll\"", &up, &types).unwrap();
        assert_eq!(
            cmd,
            r#"db.getCollection("my \"coll\"").updateOne({"_id": ObjectId("64b7f1f1f1f1f1f1f1f1f1f1")}, {"$set": {"n": 5, "name": null, "tags": ["a", 2], "note": ""}})"#
        );
        let Command::Collection {
            name,
            method,
            args,
            chain,
        } = parse_command(&cmd).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            (name.as_str(), method.as_str(), chain.len()),
            ("my \"coll\"", "updateOne", 0)
        );
        assert_eq!(args[0], Bson::Document(doc! { "_id": oid }));
        assert_eq!(
            args[1],
            Bson::Document(
                doc! { "$set": { "n": 5, "name": Bson::Null, "tags": ["a", 2], "note": "" } }
            )
        );

        let ins = Change::Insert {
            values: vec![
                kv("name", Some("Ana\nP")),
                kv("at", Some("2026-01-02T03:04:05Z")),
            ],
        };
        let cmd = change_command("users", &ins, &types).unwrap();
        let Command::Collection { method, args, .. } = parse_command(&cmd).unwrap() else {
            panic!()
        };
        assert_eq!(method, "insertOne");
        let d = args[0].as_document().unwrap();
        assert_eq!(d.get_str("name").unwrap(), "Ana\nP");
        assert_eq!(
            render(d.get("at").unwrap()).unwrap(),
            "2026-01-02T03:04:05Z"
        );
        assert_eq!(
            change_command("users", &Change::Insert { values: vec![] }, &types).unwrap(),
            r#"db.getCollection("users").insertOne({})"#
        );

        let del = Change::Delete { key: vec![id] };
        assert_eq!(
            change_command("users", &del, &types).unwrap(),
            r#"db.getCollection("users").deleteOne({"_id": ObjectId("64b7f1f1f1f1f1f1f1f1f1f1")})"#
        );
        // An _id that doesn't fit the sampled type is looked up as a string.
        let del = Change::Delete {
            key: vec![kv("_id", Some("legacy-1"))],
        };
        assert_eq!(
            change_command("users", &del, &types).unwrap(),
            r#"db.getCollection("users").deleteOne({"_id": "legacy-1"})"#
        );

        assert!(
            change_command(
                "users",
                &Change::Update {
                    key: vec![kv("_id", Some("1"))],
                    set: vec![kv("_id", Some("2"))]
                },
                &types
            )
            .is_err()
        );
        let bad = Change::Update {
            key: vec![kv("_id", Some("1"))],
            set: vec![kv("n", Some("many"))],
        };
        assert!(
            change_command("users", &bad, &types)
                .unwrap_err()
                .starts_with("n: ")
        );
        assert!(
            change_command(
                "users",
                &Change::Delete {
                    key: vec![kv("x", Some("1"))]
                },
                &types
            )
            .is_err()
        );
    }

    #[test]
    fn read_only_commands() {
        assert!(is_read_command("db.users.find({})"));
        assert!(is_read_command("db.users.aggregate([{$match: {a: 1}}])"));
        assert!(!is_read_command(
            "db.users.aggregate([{$match: {a: 1}}, {$out: 'x'}])"
        ));
        assert!(!is_read_command("db.users.deleteMany({})"));
        assert!(is_read_command("db.runCommand({dbStats: 1})"));
        assert!(!is_read_command("db.runCommand({dropDatabase: 1})"));
    }

    #[test]
    fn splits_scripts() {
        let s =
            "db.a.find({\n  x: 1\n})\n  .limit(2)\n// note\ndb.b.find(); db.c.find()\n\nshow dbs";
        let parts: Vec<&str> = split_script(s).into_iter().map(|(a, b)| &s[a..b]).collect();
        assert_eq!(
            parts,
            vec![
                "db.a.find({\n  x: 1\n})\n  .limit(2)",
                "db.b.find()",
                "db.c.find()",
                "show dbs"
            ]
        );
    }

    #[test]
    fn documents_become_rows() {
        let rs = documents_result(
            vec![
                doc! { "name": "a", "_id": 1, "tags": ["x"] },
                doc! { "_id": 2, "extra": { "k": true } },
            ],
            10,
        );
        assert_eq!(rs.columns, vec!["_id", "name", "tags", "extra"]);
        assert_eq!(
            rs.rows[0],
            vec![
                Some("1".into()),
                Some("a".into()),
                Some(r#"["x"]"#.into()),
                None
            ]
        );
        assert_eq!(rs.rows[1][3].as_deref(), Some(r#"{"k":true}"#));
        let cfg = ConnectionConfig {
            user: "a b".into(),
            password: "p@ss".into(),
            host: "db".into(),
            port: 27017,
            ..ConnectionConfig::new(1, super::super::Driver::MongoDb)
        };
        assert_eq!(
            connection_string(&cfg),
            "mongodb://a%20b:p%40ss@db:27017/?authSource=admin"
        );
    }
}
