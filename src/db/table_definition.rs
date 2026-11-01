//! Catalog-backed table definitions and faithful same-schema duplication.

use super::{Driver, ResultSet, Session};
use crate::edit::literal;
use crate::i18n::{tr, trf};

#[derive(Clone, Debug, PartialEq)]
pub struct TableDefinition {
    pub ddl: Vec<String>,
    pub columns: Vec<String>,
}

pub fn supported(driver: Driver) -> bool {
    matches!(
        driver,
        Driver::Sqlite
            | Driver::MsSql
            | Driver::MySql
            | Driver::MariaDb
            | Driver::Postgres
            | Driver::Snowflake
    )
}

fn validate_snowflake_source(sql: &str, native_duplicate: bool) -> anyhow::Result<()> {
    let ts = tokens(sql, Driver::Snowflake)?;
    anyhow::ensure!(
        ts.first().is_some_and(|t| t.is("CREATE")),
        tr("Expected catalog CREATE statement")
    );
    let first =
        if ts.get(1).is_some_and(|t| t.is("OR")) && ts.get(2).is_some_and(|t| t.is("REPLACE")) {
            3
        } else {
            1
        };
    let table = ts
        .iter()
        .enumerate()
        .skip(first)
        .find(|(_, t)| t.is("TABLE"))
        .map(|(i, _)| i)
        .ok_or_else(|| anyhow::anyhow!(tr("Expected catalog CREATE statement")))?;
    anyhow::ensure!(
        !native_duplicate || table == first,
        tr("Native Snowflake duplication requires an ordinary permanent table")
    );
    let (name_end, _, _) = object_name(&ts, table + 1)?;
    let body = name_end + 1;
    anyhow::ensure!(
        ts.get(body).is_some_and(|t| t.value == "("),
        tr("Snowflake virtual or complex column expressions require a dedicated backup")
    );
    let mut depth = 0;
    for token in ts.iter().skip(body) {
        match &sql[token.start..token.end] {
            "(" => depth += 1,
            ")" => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => anyhow::ensure!(
                depth != 1 || !token.is("AS"),
                tr("Snowflake virtual or complex column expressions require a dedicated backup")
            ),
        }
    }
    anyhow::ensure!(
        depth == 0
            && !ts.windows(2).any(|pair| pair[0].value == "$"
                && pair[1].value == "$"
                && pair[0].end == pair[1].start),
        tr("Snowflake virtual or complex column expressions require a dedicated backup")
    );
    Ok(())
}

/// Post-data constraints preserve self-references without depending on row enumeration order.
pub fn dump_ddl(
    driver: Driver,
    schema: &str,
    table: &str,
    ddl: &[String],
) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    let mut before = Vec::new();
    let mut after = Vec::new();
    for sql in ddl {
        let ts = tokens(sql, driver)?;
        if matches!(driver, Driver::Postgres | Driver::MsSql)
            && ts.first().is_some_and(|t| t.is("ALTER"))
            && ts.get(1).is_some_and(|t| t.is("TABLE"))
        {
            // Keep ADD and subsequent NOCHECK statements together and in catalog order.
            after.push(sql.clone());
        } else if matches!(driver, Driver::MySql | Driver::MariaDb | Driver::Snowflake)
            && ts.first().is_some_and(|t| t.is("CREATE"))
            && ts.iter().any(|t| t.is("TABLE"))
        {
            let keyword = ts
                .iter()
                .position(|t| t.is("TABLE"))
                .ok_or_else(|| anyhow::anyhow!(tr("Expected catalog CREATE statement")))?;
            let (end, _, _) = object_name(&ts, keyword + 1)?;
            let open = ts
                .get(end + 1)
                .filter(|t| &sql[t.start..t.end] == "(")
                .ok_or_else(|| {
                    anyhow::anyhow!(tr(
                        "Table constraints cannot be safely separated from this definition"
                    ))
                })?;
            let mut depth = 1;
            let mut start = open.end;
            let mut entries = Vec::new();
            let mut close = None;
            for token in ts.iter().skip(end + 2) {
                match &sql[token.start..token.end] {
                    "(" => depth += 1,
                    ")" => {
                        depth -= 1;
                        if depth == 0 {
                            entries.push(&sql[start..token.start]);
                            close = Some(token.start);
                            break;
                        }
                    }
                    "," if depth == 1 => {
                        entries.push(&sql[start..token.start]);
                        start = token.end;
                    }
                    _ => {}
                }
            }
            let close = close.ok_or_else(|| {
                anyhow::anyhow!(tr(
                    "Table constraints cannot be safely separated from this definition"
                ))
            })?;
            let mut kept = Vec::new();
            for entry in entries {
                anyhow::ensure!(
                    !entry.trim_start().starts_with("/*!"),
                    tr("Conditional table constraints require a dedicated backup")
                );
                let fields = tokens(entry, driver)?;
                let mut first = 0;
                if fields.first().is_some_and(|t| t.is("CONSTRAINT")) {
                    first = if fields.get(1).is_some_and(|t| t.is("FOREIGN")) {
                        1
                    } else {
                        2
                    };
                }
                if fields.get(first).is_some_and(|t| t.is("FOREIGN"))
                    && fields.get(first + 1).is_some_and(|t| t.is("KEY"))
                {
                    after.push(format!(
                        "ALTER TABLE {} ADD {}",
                        qualified(driver, schema, table),
                        entry.trim()
                    ));
                } else {
                    anyhow::ensure!(
                        !fields.iter().any(|t| t.is("REFERENCES")),
                        tr("Inline column foreign keys require a dedicated backup")
                    );
                    kept.push(entry);
                }
            }
            anyhow::ensure!(
                !kept.is_empty(),
                tr("Table constraints cannot be safely separated from this definition")
            );
            before.push(format!(
                "{}{}{}",
                &sql[..open.end],
                kept.join(","),
                &sql[close..]
            ));
        } else {
            before.push(sql.clone());
        }
    }
    Ok((before, after))
}

/// Only directly provable scopes count; role-only privileges deliberately fail closed.
fn mysql_trigger_visible(grants: &[String], schema: &str, table: &str) -> anyhow::Result<bool> {
    let mut allowed = false;
    for sql in grants {
        let ts = tokens(sql, Driver::MySql)?;
        let revoke = ts.first().is_some_and(|t| t.is("REVOKE"));
        if !revoke && !ts.first().is_some_and(|t| t.is("GRANT")) {
            continue;
        }
        let Some(on) = ts.iter().position(|t| t.is("ON")) else {
            continue;
        };
        let mut depth = 0;
        let mut trigger = false;
        for privilege in &ts[1..on] {
            match &sql[privilege.start..privilege.end] {
                "(" => depth += 1,
                ")" => depth -= 1,
                _ => trigger |= depth == 0 && (privilege.is("TRIGGER") || privilege.is("ALL")),
            }
        }
        if !trigger {
            continue;
        }
        let Some(scope) = ts.get(on + 1..on + 4) else {
            continue;
        };
        if &sql[scope[1].start..scope[1].end] != "." {
            continue;
        }
        let wildcard = |token: &Token| &sql[token.start..token.end] == "*";
        let global = wildcard(&scope[0]) && wildcard(&scope[2]);
        let scoped = scope[0].value == schema
            && (wildcard(&scope[2]) || scope[2].value == table)
            && !schema.contains('\\');
        if global || scoped {
            if revoke {
                return Ok(false);
            }
            allowed = true;
        }
    }
    Ok(allowed)
}

pub async fn definition(
    session: &mut Session,
    driver: Driver,
    schema: &str,
    table: &str,
) -> anyhow::Result<TableDefinition> {
    validate_names(schema, table)?;
    catalog(session, driver, schema, table, table).await
}

/// Nontransactional plans require the worker to clean up a successfully created target on failure.
pub async fn duplicate_plan(
    session: &mut Session,
    driver: Driver,
    schema: &str,
    source: &str,
    target: &str,
    data: bool,
) -> anyhow::Result<super::alter::Plan> {
    validate_names(schema, source)?;
    validate_names(schema, target)?;
    anyhow::ensure!(
        source != target,
        tr("The duplicate needs a different table name")
    );
    let exists = match driver {
        Driver::Sqlite => format!(
            "SELECT name FROM {}.sqlite_schema WHERE name = {} COLLATE NOCASE",
            driver.quote_ident(schema),
            lit(driver, target)
        ),
        Driver::MsSql => format!(
            "SELECT name FROM sys.objects WHERE schema_id = SCHEMA_ID({}) AND name = {}",
            lit(driver, schema),
            lit(driver, target)
        ),
        Driver::Postgres => format!(
            "SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname={} AND c.relname={}",
            lit(driver, schema),
            lit(driver, target)
        ),
        Driver::MySql | Driver::MariaDb | Driver::Snowflake => format!(
            "SELECT table_name FROM information_schema.tables WHERE table_schema={} AND table_name={}",
            lit(driver, schema),
            lit(driver, target)
        ),
        _ => anyhow::bail!(trf(
            "Faithful table duplication is not available for {engine}",
            &[("engine", &driver.label())]
        )),
    };
    anyhow::ensure!(
        query(session, &exists).await?.rows.is_empty(),
        tr("The destination table or object already exists")
    );
    let def = catalog(session, driver, schema, source, target).await?;
    let from = qualified(driver, schema, source);
    let to = qualified(driver, schema, target);
    let transactional = matches!(driver, Driver::Sqlite | Driver::Postgres | Driver::MsSql);
    let mut plan = super::alter::Plan {
        statements: def.ddl,
        transaction: transactional,
        warnings: Vec::new(),
    };
    if driver == Driver::Snowflake {
        plan.statements = vec![format!(
            "CREATE TABLE {to} {} {from}",
            if data { "CLONE" } else { "LIKE" }
        )];
    } else if data {
        anyhow::ensure!(
            !def.columns.is_empty(),
            tr("The table has no insertable columns")
        );
        let options = data_insert_options(session, driver, schema, source).await?;
        let mut columns = def.columns;
        if driver == Driver::Sqlite {
            let tables = query(
                session,
                &format!(
                    "SELECT wr FROM pragma_table_list WHERE schema={} AND name={} COLLATE NOCASE",
                    lit(driver, schema),
                    lit(driver, source)
                ),
            )
            .await?;
            if tables.rows.first().and_then(|r| optional(r, 0)) == Some("0") {
                let fields = query(
                    session,
                    &format!(
                        "SELECT name FROM pragma_table_xinfo({}, {})",
                        lit(driver, source),
                        lit(driver, schema)
                    ),
                )
                .await?;
                if let Some(alias) = ["rowid", "_rowid_", "oid"].into_iter().find(|alias| {
                    !fields
                        .rows
                        .iter()
                        .any(|r| optional(r, 0).is_some_and(|n| n.eq_ignore_ascii_case(alias)))
                }) {
                    columns.push(alias.to_string());
                }
            }
        }
        let cols = columns
            .iter()
            .map(|c| driver.quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ");
        let overriding = if options.overriding_system_value {
            " OVERRIDING SYSTEM VALUE"
        } else {
            ""
        };
        // Metadata options reference the source for dumps; duplication targets its new table.
        let before = options
            .before
            .into_iter()
            .map(|sql| retarget_option(&sql, driver, schema, source, target))
            .collect::<Vec<_>>();
        let after = options
            .after
            .into_iter()
            .map(|sql| retarget_option(&sql, driver, schema, source, target))
            .collect::<Vec<_>>();
        if driver == Driver::MsSql && !before.is_empty() {
            // IDENTITY_INSERT is session state, so restore it even when the INSERT raises an error.
            plan.statements.push(format!("BEGIN TRY\n{};\nINSERT INTO {to} ({cols}) SELECT {cols} FROM {from};\n{};\nEND TRY\nBEGIN CATCH\nSET IDENTITY_INSERT {to} OFF;\nTHROW;\nEND CATCH",before.join(";\n"),after.join(";\n")));
        } else {
            plan.statements.extend(before);
            plan.statements.push(format!(
                "INSERT INTO {to} ({cols}){overriding} SELECT {cols} FROM {from}"
            ));
            plan.statements.extend(after);
        }
        if driver == Driver::Sqlite
            && plan.statements.first().is_some_and(|sql| {
                tokens(sql, driver).is_ok_and(|ts| ts.iter().any(|t| t.is("AUTOINCREMENT")))
            })
        {
            let sequence = format!("{}.sqlite_sequence", driver.quote_ident(schema));
            plan.statements.push(format!(
                "DELETE FROM {sequence} WHERE name={}",
                lit(driver, target)
            ));
            plan.statements.push(format!(
                "INSERT INTO {sequence}(name,seq) SELECT {},seq FROM {sequence} WHERE name={}",
                lit(driver, target),
                lit(driver, source)
            ));
        }
    }
    if !transactional {
        plan.warnings.push(tr("This engine commits table creation immediately. If a later statement fails, the newly created table must be removed.").into());
    }
    Ok(plan)
}

fn validate_names(schema: &str, table: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !schema.is_empty()
            && !table.trim().is_empty()
            && !schema.contains('\0')
            && !table.contains('\0'),
        tr("A schema and a nonempty table name without NUL characters are required")
    );
    Ok(())
}

fn lit(driver: Driver, value: &str) -> String {
    if driver == Driver::Postgres {
        // E strings keep catalog lookups safe even with standard_conforming_strings disabled.
        return format!("E'{}'", value.replace('\\', "\\\\").replace('\'', "''"));
    }
    literal(driver, &Some(value.to_string()))
}

fn fidelity_reason(reason: &str) -> &str {
    match reason {
        "VIEW DEFINITION permission is required" => tr("VIEW DEFINITION permission is required"),
        "temporal, memory-optimized or FILESTREAM table" => {
            tr("temporal, memory-optimized or FILESTREAM table")
        }
        "table triggers" => tr("table triggers"),
        "row-level security" => tr("row-level security"),
        "specialized, rowversion, alias, legacy padding or inaccessible column definition" => {
            tr("specialized, rowversion, alias, legacy padding or inaccessible column definition")
        }
        "replication identity" => tr("replication identity"),
        "specialized, disabled or partitioned index" => {
            tr("specialized, disabled or partitioned index")
        }
        "table compression" => tr("table compression"),
        "partitioning or inheritance" => tr("partitioning or inheritance"),
        "row-level security policies" => tr("row-level security policies"),
        "table rules" => tr("table rules"),
        "shared sequence defaults" => tr("shared sequence defaults"),
        "custom replica identity" => tr("custom replica identity"),
        _ => reason,
    }
}

async fn query(session: &mut Session, sql: &str) -> anyhow::Result<ResultSet> {
    let set = session
        .execute(sql, 100_000)
        .await?
        .into_iter()
        .find(|s| !s.columns.is_empty())
        .ok_or_else(|| anyhow::anyhow!(tr("The table catalog returned no result set")))?;
    anyhow::ensure!(
        !set.truncated,
        tr("The table definition exceeds the catalog row limit")
    );
    Ok(set)
}

async fn catalog(
    session: &mut Session,
    driver: Driver,
    schema: &str,
    source: &str,
    target: &str,
) -> anyhow::Result<TableDefinition> {
    match driver {
        Driver::Sqlite => sqlite_definition(session, schema, source, target).await,
        Driver::MsSql => mssql_definition(session, schema, source, target).await,
        Driver::Postgres => postgres_definition(session, schema, source, target).await,
        Driver::MySql | Driver::MariaDb => {
            mysql_definition(session, driver, schema, source, target).await
        }
        Driver::Snowflake => {
            let name = qualified(driver, schema, source);
            let ddl = query(
                session,
                &format!("SELECT GET_DDL('TABLE',{})", lit(driver, &name)),
            )
            .await?;
            let text = ddl
                .rows
                .first()
                .map(|r| cell(r, 0))
                .transpose()?
                .ok_or_else(|| anyhow::anyhow!(tr("Table not found")))?;
            validate_snowflake_source(text, source != target)?;
            // GET_DDL can contain CREATE OR REPLACE: exporting it could overwrite a table.
            let ts = tokens(text, driver)?;
            anyhow::ensure!(
                !ts.iter().any(|t| t.is("POLICY") || t.is("TAG")),
                tr("Snowflake policies and tags require a dedicated backup")
            );
            let text = if ts.get(1).is_some_and(|t| t.is("OR"))
                && ts.get(2).is_some_and(|t| t.is("REPLACE"))
            {
                apply_replacements(text, vec![(ts[1].start, ts[2].end, String::new())])?
            } else {
                text.to_string()
            };
            let cols = query(session,&format!("SELECT column_name FROM information_schema.columns WHERE table_schema={} AND table_name={} ORDER BY ordinal_position",lit(driver,schema),lit(driver,source))).await?;
            Ok(TableDefinition {
                ddl: vec![rewrite_create(&text, driver, schema, source, target, None)?],
                columns: cols
                    .rows
                    .iter()
                    .map(|r| cell(r, 0).map(str::to_string))
                    .collect::<anyhow::Result<_>>()?,
            })
        }
        _ => anyhow::bail!(trf(
            "Faithful table definitions are not available for {engine}",
            &[("engine", &driver.label())]
        )),
    }
}

async fn sqlite_definition(
    session: &mut Session,
    schema: &str,
    source: &str,
    target: &str,
) -> anyhow::Result<TableDefinition> {
    let driver = Driver::Sqlite;
    let objects = query(session,&format!("SELECT type,name,sql FROM {}.sqlite_schema WHERE tbl_name={} COLLATE NOCASE ORDER BY CASE type WHEN 'table' THEN 0 ELSE 1 END,name",driver.quote_ident(schema),lit(driver,source))).await?;
    let table = objects
        .rows
        .iter()
        .find(|r| optional(r, 0) == Some("table"))
        .ok_or_else(|| anyhow::anyhow!(tr("Table not found")))?;
    let sql = cell(table, 2)?;
    let ts = tokens(sql, driver)?;
    anyhow::ensure!(
        !ts.iter().take(4).any(|t| t.is("VIRTUAL")),
        tr("Virtual tables require their extension's backup procedure")
    );
    anyhow::ensure!(
        !objects
            .rows
            .iter()
            .any(|r| optional(r, 0) == Some("trigger")),
        tr(
            "Tables with triggers require a dedicated backup; trigger bodies cannot be safely retargeted"
        )
    );
    let mut ddl = vec![rewrite_create(sql, driver, schema, source, target, None)?];
    for row in objects
        .rows
        .iter()
        .filter(|r| optional(r, 0) == Some("index") && optional(r, 2).is_some())
    {
        let old = cell(row, 1)?;
        let name = if source == target {
            old.to_string()
        } else {
            format!("{target}__{old}")
        };
        ddl.push(rewrite_create(
            cell(row, 2)?,
            driver,
            schema,
            source,
            target,
            Some(&name),
        )?);
    }
    let cols = query(
        session,
        &format!(
            "SELECT name,hidden FROM pragma_table_xinfo({}, {}) ORDER BY cid",
            lit(driver, source),
            lit(driver, schema)
        ),
    )
    .await?;
    let columns = cols
        .rows
        .iter()
        .filter(|r| optional(r, 1) == Some("0"))
        .map(|r| cell(r, 0).map(str::to_string))
        .collect::<anyhow::Result<Vec<_>>>()?;
    anyhow::ensure!(
        !columns.is_empty(),
        tr("No writable columns were found in the table catalog")
    );
    Ok(TableDefinition { ddl, columns })
}

async fn mysql_definition(
    session: &mut Session,
    driver: Driver,
    schema: &str,
    source: &str,
    target: &str,
) -> anyhow::Result<TableDefinition> {
    let sch = lit(driver, schema);
    let tab = lit(driver, source);
    let mode = query(session, "SELECT @@SESSION.sql_mode").await?;
    anyhow::ensure!(
        !mode
            .rows
            .first()
            .and_then(|r| optional(r, 0))
            .is_some_and(|m| m.split(',').any(|m| m == "NO_BACKSLASH_ESCAPES")),
        tr("Table definitions with NO_BACKSLASH_ESCAPES require a dedicated backup")
    );
    let grants = query(session, "SHOW GRANTS")
        .await?
        .rows
        .iter()
        .map(|r| cell(r, 0).map(str::to_string))
        .collect::<anyhow::Result<Vec<_>>>()?;
    anyhow::ensure!(
        mysql_trigger_visible(&grants, schema, source)?,
        tr("A direct TRIGGER or ALL grant is required to verify trigger metadata")
    );
    let triggers = query(session,&format!("SELECT trigger_name FROM information_schema.triggers WHERE event_object_schema={sch} AND event_object_table={tab}")).await?;
    anyhow::ensure!(
        triggers.rows.is_empty(),
        tr("Tables with triggers require a dedicated backup")
    );
    let create = query(
        session,
        &format!("SHOW CREATE TABLE {}", qualified(driver, schema, source)),
    )
    .await?;
    let ddl = create
        .rows
        .first()
        .map(|r| cell(r, 1))
        .transpose()?
        .ok_or_else(|| anyhow::anyhow!(tr("Table not found")))?;
    let ts = tokens(ddl, driver)?;
    anyhow::ensure!(
        ts.get(1).is_some_and(|t| t.is("TABLE")),
        tr("Only ordinary tables can be duplicated")
    );
    anyhow::ensure!(
        !ts.iter()
            .any(|t| t.is("TABLESPACE") || t.is("DIRECTORY") || t.is("SYSTEM")),
        tr(
            "Tables with tablespaces, external directories or system versioning require a dedicated backup"
        )
    );
    let columns = query(session,&format!("SELECT column_name FROM information_schema.columns WHERE table_schema={sch} AND table_name={tab} AND COALESCE(generation_expression,'')='' ORDER BY ordinal_position")).await?;
    Ok(TableDefinition {
        ddl: vec![rewrite_create(ddl, driver, schema, source, target, None)?],
        columns: columns
            .rows
            .iter()
            .map(|r| cell(r, 0).map(str::to_string))
            .collect::<anyhow::Result<_>>()?,
    })
}

async fn mssql_definition(
    session: &mut Session,
    schema: &str,
    source: &str,
    target: &str,
) -> anyhow::Result<TableDefinition> {
    let d = Driver::MsSql;
    let from = qualified(d, schema, source);
    let to = qualified(d, schema, target);
    let object = format!("OBJECT_ID({})", lit(d, &from));
    let gate = query(session,&format!("SELECT t.object_id, CASE \
        WHEN COALESCE(HAS_PERMS_BY_NAME({},'OBJECT','VIEW DEFINITION'),0)<>1 THEN 'VIEW DEFINITION permission is required' \
        WHEN t.temporal_type<>0 OR t.is_memory_optimized=1 OR t.is_filetable=1 OR t.filestream_data_space_id>0 THEN 'temporal, memory-optimized or FILESTREAM table' \
        WHEN EXISTS(SELECT 1 FROM sys.triggers WHERE parent_id=t.object_id) THEN 'table triggers' \
        WHEN EXISTS(SELECT 1 FROM sys.security_predicates WHERE target_object_id=t.object_id) THEN 'row-level security' \
        WHEN EXISTS(SELECT 1 FROM sys.columns c WHERE c.object_id=t.object_id AND (c.is_sparse=1 OR c.is_column_set=1 OR c.is_filestream=1 OR c.xml_collection_id<>0 OR c.generated_always_type<>0 OR c.encryption_type IS NOT NULL OR c.is_hidden=1 OR c.is_masked=1 OR c.user_type_id<>c.system_type_id OR c.system_type_id=189 OR c.rule_object_id<>0 OR (c.is_ansi_padded=0 AND c.system_type_id IN(165,167,173,175)) OR (c.default_object_id<>0 AND OBJECT_DEFINITION(c.default_object_id) IS NULL))) THEN 'specialized, rowversion, alias, legacy padding or inaccessible column definition' \
        WHEN EXISTS(SELECT 1 FROM sys.identity_columns WHERE object_id=t.object_id AND is_not_for_replication=1) THEN 'replication identity' \
        WHEN EXISTS(SELECT 1 FROM sys.indexes i LEFT JOIN sys.data_spaces ds ON ds.data_space_id=i.data_space_id WHERE i.object_id=t.object_id AND (i.type NOT IN(0,1,2) OR i.is_disabled=1 OR i.is_hypothetical=1 OR ds.type='PS' OR (i.has_filter=1 AND i.filter_definition IS NULL))) THEN 'specialized, disabled or partitioned index' \
        WHEN EXISTS(SELECT 1 FROM sys.partitions WHERE object_id=t.object_id AND data_compression<>0) THEN 'table compression' \
        ELSE '' END FROM sys.tables t WHERE t.object_id={object}",lit(d,&from))).await?;
    let row = gate
        .rows
        .first()
        .ok_or_else(|| anyhow::anyhow!(tr("Table not found or catalog metadata is not visible")))?;
    anyhow::ensure!(
        cell(row, 1)?.is_empty(),
        trf(
            "Faithful SQL Server definition requires a dedicated backup: {reason}",
            &[("reason", &fidelity_reason(cell(row, 1)?))]
        )
    );
    let cols = query(session,&format!("SELECT c.name,TYPE_NAME(c.system_type_id),c.max_length,c.precision,c.scale,c.is_nullable,c.collation_name,OBJECT_DEFINITION(c.default_object_id), \
        CONVERT(varchar(100),ic.seed_value),CONVERT(varchar(100),ic.increment_value),cc.definition,COALESCE(cc.is_persisted,0),c.is_rowguidcol,dc.name \
        FROM sys.columns c LEFT JOIN sys.identity_columns ic ON ic.object_id=c.object_id AND ic.column_id=c.column_id \
        LEFT JOIN sys.computed_columns cc ON cc.object_id=c.object_id AND cc.column_id=c.column_id \
        LEFT JOIN sys.default_constraints dc ON dc.object_id=c.default_object_id WHERE c.object_id={object} ORDER BY c.column_id")).await?;
    anyhow::ensure!(
        !cols.rows.is_empty(),
        tr("Table has no visible column definitions")
    );
    let mut definitions = Vec::new();
    let mut columns = Vec::new();
    for row in &cols.rows {
        let default_name = optional(row, 13).map(|name| {
            if source == target {
                name.to_string()
            } else {
                distinct_name(target, name)
            }
        });
        let (sql, _) = mssql_column(row, default_name.as_deref())?;
        if optional(row, 10).is_none() {
            columns.push(cell(row, 0)?.to_string());
        }
        definitions.push(sql);
    }
    let mut ddl = vec!["SET ANSI_NULLS ON; SET QUOTED_IDENTIFIER ON; SET ANSI_PADDING ON; SET ANSI_WARNINGS ON; SET ARITHABORT ON; SET CONCAT_NULL_YIELDS_NULL ON; SET NUMERIC_ROUNDABORT OFF".into(),format!(
        "CREATE TABLE {to} (\n    {}\n)",
        definitions.join(",\n    ")
    )];
    let indexes=query(session,&format!("SELECT name,type,is_unique,is_primary_key,is_unique_constraint,filter_definition,fill_factor,is_padded,ignore_dup_key,allow_row_locks,allow_page_locks FROM sys.indexes WHERE object_id={object} AND type IN(1,2) ORDER BY is_primary_key DESC,is_unique_constraint DESC,index_id")).await?;
    for row in indexes.rows {
        let old = cell(&row, 0)?;
        let name = if source == target {
            old.to_string()
        } else {
            distinct_name(target, old)
        };
        let fields=query(session,&format!("SELECT c.name,ic.is_descending_key,ic.is_included_column FROM sys.index_columns ic \
            JOIN sys.columns c ON c.object_id=ic.object_id AND c.column_id=ic.column_id JOIN sys.indexes i ON i.object_id=ic.object_id AND i.index_id=ic.index_id \
            WHERE ic.object_id={object} AND i.name={} AND (ic.key_ordinal>0 OR ic.is_included_column=1) ORDER BY ic.key_ordinal,ic.index_column_id",lit(d,old))).await?;
        let mut keys = Vec::new();
        let mut included = Vec::new();
        for field in fields.rows {
            let col = d.quote_ident(cell(&field, 0)?);
            if truth(cell(&field, 2)?) {
                included.push(col);
            } else {
                keys.push(format!(
                    "{col} {}",
                    if truth(cell(&field, 1)?) {
                        "DESC"
                    } else {
                        "ASC"
                    }
                ));
            }
        }
        anyhow::ensure!(
            !keys.is_empty(),
            trf("Index {name} has no visible key columns", &[("name", &old)])
        );
        let clustered = if cell(&row, 1)? == "1" {
            "CLUSTERED"
        } else {
            "NONCLUSTERED"
        };
        let options = format!(
            "WITH (FILLFACTOR={}, PAD_INDEX={}, IGNORE_DUP_KEY={}, ALLOW_ROW_LOCKS={}, ALLOW_PAGE_LOCKS={})",
            number(cell(&row, 6)?)?,
            onoff(cell(&row, 7)?),
            onoff(cell(&row, 8)?),
            onoff(cell(&row, 9)?),
            onoff(cell(&row, 10)?)
        );
        let statement = if truth(cell(&row, 3)?) || truth(cell(&row, 4)?) {
            format!(
                "ALTER TABLE {to} ADD CONSTRAINT {} {} {clustered} ({}) {options}",
                d.quote_ident(&name),
                if truth(cell(&row, 3)?) {
                    "PRIMARY KEY"
                } else {
                    "UNIQUE"
                },
                keys.join(", ")
            )
        } else {
            let include = if included.is_empty() {
                String::new()
            } else {
                format!(" INCLUDE ({})", included.join(", "))
            };
            let filter = optional(&row, 5)
                .map(|s| format!(" WHERE {s}"))
                .unwrap_or_default();
            format!(
                "CREATE {}{clustered} INDEX {} ON {to} ({}){include}{filter} {options}",
                if truth(cell(&row, 2)?) { "UNIQUE " } else { "" },
                d.quote_ident(&name),
                keys.join(", ")
            )
        };
        ddl.push(statement);
    }
    let checks=query(session,&format!("SELECT name,definition,is_disabled,is_not_trusted,is_not_for_replication FROM sys.check_constraints WHERE parent_object_id={object} ORDER BY object_id")).await?;
    for row in checks.rows {
        let name = if source == target {
            cell(&row, 0)?.to_string()
        } else {
            distinct_name(target, cell(&row, 0)?)
        };
        let name = d.quote_ident(&name);
        ddl.push(format!(
            "ALTER TABLE {to} WITH {} ADD CONSTRAINT {name} CHECK {}{}",
            if truth(cell(&row, 3)?) {
                "NOCHECK"
            } else {
                "CHECK"
            },
            if truth(cell(&row, 4)?) {
                "NOT FOR REPLICATION "
            } else {
                ""
            },
            cell(&row, 1)?
        ));
        if truth(cell(&row, 2)?) {
            ddl.push(format!("ALTER TABLE {to} NOCHECK CONSTRAINT {name}"));
        }
    }
    let foreign=query(session,&format!("SELECT fk.name,SCHEMA_NAME(rt.schema_id),rt.name,fk.delete_referential_action_desc,fk.update_referential_action_desc,fk.is_disabled,fk.is_not_trusted,fk.is_not_for_replication \
        FROM sys.foreign_keys fk JOIN sys.tables rt ON rt.object_id=fk.referenced_object_id WHERE fk.parent_object_id={object} ORDER BY fk.object_id")).await?;
    for row in foreign.rows {
        let old = cell(&row, 0)?;
        let fields=query(session,&format!("SELECT pc.name,rc.name FROM sys.foreign_key_columns fkc JOIN sys.foreign_keys fk ON fk.object_id=fkc.constraint_object_id \
            JOIN sys.columns pc ON pc.object_id=fkc.parent_object_id AND pc.column_id=fkc.parent_column_id JOIN sys.columns rc ON rc.object_id=fkc.referenced_object_id AND rc.column_id=fkc.referenced_column_id \
            WHERE fk.parent_object_id={object} AND fk.name={} ORDER BY fkc.constraint_column_id",lit(d,old))).await?;
        anyhow::ensure!(
            !fields.rows.is_empty(),
            trf(
                "Foreign key {name} has no visible columns",
                &[("name", &old)]
            )
        );
        let local = fields
            .rows
            .iter()
            .map(|r| cell(r, 0).map(|c| d.quote_ident(c)))
            .collect::<anyhow::Result<Vec<_>>>()?
            .join(", ");
        let remote = fields
            .rows
            .iter()
            .map(|r| cell(r, 1).map(|c| d.quote_ident(c)))
            .collect::<anyhow::Result<Vec<_>>>()?
            .join(", ");
        let reference = if cell(&row, 1)? == schema && cell(&row, 2)? == source {
            to.clone()
        } else {
            qualified(d, cell(&row, 1)?, cell(&row, 2)?)
        };
        let name = d.quote_ident(&if source == target {
            old.to_string()
        } else {
            distinct_name(target, old)
        });
        let action = |value: &str| -> anyhow::Result<String> {
            anyhow::ensure!(
                matches!(value, "NO_ACTION" | "CASCADE" | "SET_NULL" | "SET_DEFAULT"),
                tr("Unknown foreign-key action")
            );
            Ok(value.replace('_', " "))
        };
        ddl.push(format!("ALTER TABLE {to} WITH {} ADD CONSTRAINT {name} FOREIGN KEY ({local}) REFERENCES {reference} ({remote}) ON DELETE {} ON UPDATE {}{}",if truth(cell(&row,6)?) {"NOCHECK"} else {"CHECK"},action(cell(&row,3)?)?,action(cell(&row,4)?)?,if truth(cell(&row,7)?) {" NOT FOR REPLICATION"} else {""}));
        if truth(cell(&row, 5)?) {
            ddl.push(format!("ALTER TABLE {to} NOCHECK CONSTRAINT {name}"));
        }
    }
    Ok(TableDefinition { ddl, columns })
}

fn onoff(value: &str) -> &'static str {
    if truth(value) { "ON" } else { "OFF" }
}

// shortcut: catalog dumps omit table/column comments, custom column storage/compression and
// unlogged serial-sequence persistence; extend these queries before supporting those properties.
async fn postgres_definition(
    session: &mut Session,
    schema: &str,
    source: &str,
    target: &str,
) -> anyhow::Result<TableDefinition> {
    let d = Driver::Postgres;
    let from = qualified(d, schema, source);
    let to = qualified(d, schema, target);
    let oid = format!("to_regclass({})", lit(d, &from));
    let gate=query(session,&format!("SELECT c.relpersistence, CASE \
        WHEN c.relkind<>'r' OR c.relispartition OR EXISTS(SELECT 1 FROM pg_inherits WHERE inhrelid=c.oid OR inhparent=c.oid) THEN 'partitioning or inheritance' \
        WHEN c.relrowsecurity OR c.relforcerowsecurity THEN 'row-level security policies' \
        WHEN EXISTS(SELECT 1 FROM pg_trigger WHERE tgrelid=c.oid AND NOT tgisinternal) THEN 'table triggers' \
        WHEN EXISTS(SELECT 1 FROM pg_rewrite WHERE ev_class=c.oid) THEN 'table rules' \
        WHEN EXISTS(SELECT 1 FROM pg_attribute a JOIN pg_attrdef ad ON ad.adrelid=a.attrelid AND ad.adnum=a.attnum WHERE a.attrelid=c.oid AND a.attidentity='' AND pg_get_expr(ad.adbin,ad.adrelid) LIKE '%nextval(%' AND pg_get_serial_sequence({},a.attname) IS NULL) THEN 'shared sequence defaults' \
        WHEN c.relreplident<>'d' THEN 'custom replica identity' \
        ELSE '' END,ts.spcname,array_to_string(c.reloptions,', '),am.amname \
        FROM pg_class c LEFT JOIN pg_tablespace ts ON ts.oid=c.reltablespace LEFT JOIN pg_am am ON am.oid=c.relam WHERE c.oid={oid}",lit(d,&from))).await?;
    let table = gate
        .rows
        .first()
        .ok_or_else(|| anyhow::anyhow!(tr("Table not found")))?;
    anyhow::ensure!(
        cell(table, 1)?.is_empty(),
        trf(
            "Faithful PostgreSQL definition requires a dedicated backup: {reason}",
            &[("reason", &fidelity_reason(cell(table, 1)?))]
        )
    );
    anyhow::ensure!(
        optional(table, 4) == Some("heap"),
        tr("Tables with custom PostgreSQL access methods require a dedicated backup")
    );
    let cols=query(session,&format!("SELECT a.attname,format_type(a.atttypid,a.atttypmod),a.attnotnull,pg_get_expr(ad.adbin,ad.adrelid),a.attgenerated,a.attidentity, \
        CASE WHEN a.attcollation<>ty.typcollation THEN quote_ident(cn.nspname)||'.'||quote_ident(co.collname) END, \
        seq.seqstart,seq.seqincrement,seq.seqmin,seq.seqmax,seq.seqcache,seq.seqcycle,ns.nspname,sc.relname,format_type(seq.seqtypid,NULL) \
        FROM pg_attribute a JOIN pg_type ty ON ty.oid=a.atttypid LEFT JOIN pg_attrdef ad ON ad.adrelid=a.attrelid AND ad.adnum=a.attnum \
        LEFT JOIN pg_collation co ON co.oid=a.attcollation LEFT JOIN pg_namespace cn ON cn.oid=co.collnamespace \
        LEFT JOIN pg_sequence seq ON seq.seqrelid=pg_get_serial_sequence({},a.attname)::regclass \
        LEFT JOIN pg_class sc ON sc.oid=seq.seqrelid LEFT JOIN pg_namespace ns ON ns.oid=sc.relnamespace \
        WHERE a.attrelid={oid} AND a.attnum>0 AND NOT a.attisdropped ORDER BY a.attnum",lit(d,&from))).await?;
    let mut fields = Vec::new();
    let mut columns = Vec::new();
    let mut sequences = Vec::new();
    let mut ownership = Vec::new();
    for row in cols.rows {
        let mut field = format!("{} {}", d.quote_ident(cell(&row, 0)?), cell(&row, 1)?);
        if let Some(collation) = optional(&row, 6) {
            field.push_str(&format!(" COLLATE {collation}"));
        }
        let generated = cell(&row, 4)?;
        let identity = cell(&row, 5)?;
        anyhow::ensure!(
            !identity.is_empty() || optional(&row, 14).is_none() || optional(&row, 3).is_some(),
            tr("Owned sequences with custom default expressions require a dedicated backup")
        );
        if !generated.is_empty() {
            anyhow::ensure!(
                matches!(generated, "s" | "v"),
                tr("Unknown generated-column kind")
            );
            field.push_str(&format!(
                " GENERATED ALWAYS AS ({}) {}",
                cell(&row, 3)?,
                if generated == "s" {
                    "STORED"
                } else {
                    "VIRTUAL"
                }
            ));
        } else {
            columns.push(cell(&row, 0)?.to_string());
            if !identity.is_empty() {
                anyhow::ensure!(
                    matches!(identity, "a" | "d"),
                    tr("Unknown identity-column kind")
                );
                field.push_str(&format!(" GENERATED {} AS IDENTITY (START WITH {} INCREMENT BY {} MINVALUE {} MAXVALUE {} CACHE {} {})",if identity=="a" {"ALWAYS"} else {"BY DEFAULT"},number(cell(&row,7)?)?,number(cell(&row,8)?)?,number(cell(&row,9)?)?,number(cell(&row,10)?)?,number(cell(&row,11)?)?,if truth(cell(&row,12)?) {"CYCLE"} else {"NO CYCLE"}));
            } else if let Some(default) = optional(&row, 3) {
                if let Some(seq_name) = optional(&row, 14) {
                    let expression = tokens(default, d)?;
                    anyhow::ensure!(
                        expression.len() == 7
                            && expression[0].is("nextval")
                            && expression[1].value == "("
                            && expression[3].value == ":"
                            && expression[4].value == ":"
                            && expression[5].is("regclass")
                            && expression[6].value == ")",
                        tr(
                            "Owned sequences with custom default expressions require a dedicated backup"
                        )
                    );
                    let seq_name = if source == target {
                        seq_name.to_string()
                    } else {
                        distinct_name(target, seq_name)
                    };
                    let seq_schema = if source == target {
                        cell(&row, 13)?
                    } else {
                        schema
                    };
                    let seq = qualified(d, seq_schema, &seq_name);
                    let sequence_type = cell(&row, 15)?;
                    anyhow::ensure!(
                        matches!(sequence_type, "smallint" | "integer" | "bigint"),
                        tr("Unknown sequence data type")
                    );
                    sequences.push(format!("CREATE SEQUENCE {seq} AS {sequence_type} START WITH {} INCREMENT BY {} MINVALUE {} MAXVALUE {} CACHE {} {}",number(cell(&row,7)?)?,number(cell(&row,8)?)?,number(cell(&row,9)?)?,number(cell(&row,10)?)?,number(cell(&row,11)?)?,if truth(cell(&row,12)?) {"CYCLE"} else {"NO CYCLE"}));
                    ownership.push(format!(
                        "ALTER SEQUENCE {seq} OWNED BY {to}.{}",
                        d.quote_ident(cell(&row, 0)?)
                    ));
                    field.push_str(&format!(" DEFAULT nextval({}::regclass)", lit(d, &seq)));
                } else {
                    field.push_str(&format!(" DEFAULT {default}"));
                }
            }
        }
        if truth(cell(&row, 2)?) {
            field.push_str(" NOT NULL");
        }
        fields.push(field);
    }
    anyhow::ensure!(!fields.is_empty(), tr("Table has no visible columns"));
    let persistence = match cell(table, 0)? {
        "u" => "UNLOGGED ",
        "t" => "TEMPORARY ",
        _ => "",
    };
    anyhow::ensure!(
        cell(table, 0)? != "t",
        tr("Temporary tables cannot be duplicated from a dedicated session")
    );
    let mut create = format!(
        "CREATE {persistence}TABLE {to} (\n    {}\n)",
        fields.join(",\n    ")
    );
    if let Some(options) = optional(table, 3).filter(|s| !s.is_empty()) {
        create.push_str(&format!(" WITH ({options})"));
    }
    if let Some(tablespace) = optional(table, 2) {
        create.push_str(&format!(" TABLESPACE {}", d.quote_ident(tablespace)));
    }
    let has_serial = !sequences.is_empty();
    let mut ddl = sequences;
    ddl.push(create);
    ddl.extend(ownership);
    let constraints=query(session,&format!("SELECT conname,pg_get_constraintdef(oid,false),contype FROM pg_constraint WHERE conrelid={oid} AND contype IN('p','u','x','c','f') ORDER BY CASE contype WHEN 'p' THEN 0 WHEN 'u' THEN 1 WHEN 'x' THEN 2 WHEN 'c' THEN 3 ELSE 4 END,conname")).await?;
    for row in constraints.rows {
        let definition = cell(&row, 1)?;
        let rewritten = apply_replacements(
            definition,
            reference_edits(&tokens(definition, d)?, d, schema, source, target)?,
        )?;
        let name = if source == target {
            cell(&row, 0)?.to_string()
        } else {
            distinct_name(target, cell(&row, 0)?)
        };
        ddl.push(format!(
            "ALTER TABLE {to} ADD CONSTRAINT {} {rewritten}",
            d.quote_ident(&name)
        ));
    }
    let indexes=query(session,&format!("SELECT ci.relname,pg_get_indexdef(i.indexrelid,0,false),i.indisvalid,i.indisready FROM pg_index i JOIN pg_class ci ON ci.oid=i.indexrelid \
        WHERE i.indrelid={oid} AND NOT EXISTS(SELECT 1 FROM pg_constraint con WHERE con.conindid=i.indexrelid AND con.contype IN('p','u','x')) ORDER BY ci.relname")).await?;
    for row in indexes.rows {
        anyhow::ensure!(
            truth(cell(&row, 2)?) && truth(cell(&row, 3)?),
            tr("Invalid or unready PostgreSQL indexes require a dedicated backup")
        );
        let name = if source == target {
            cell(&row, 0)?.to_string()
        } else {
            distinct_name(target, cell(&row, 0)?)
        };
        // PostgreSQL index names cannot be schema-qualified in CREATE INDEX.
        let sql = rewrite_create(cell(&row, 1)?, d, schema, source, target, Some(&name))?;
        let ts = tokens(&sql, d)?;
        let pos = ts
            .iter()
            .position(|t| t.is("INDEX"))
            .ok_or_else(|| anyhow::anyhow!(tr("Missing index definition")))?
            + 1;
        let (end, _, _) = object_name(&ts, pos)?;
        ddl.push(apply_replacements(
            &sql,
            vec![(ts[pos].start, ts[end].end, d.quote_ident(&name))],
        )?);
    }
    if source != target && !has_serial {
        // Native LIKE preserves index expressions, identity options, storage and comments.
        // Foreign keys are the constraint kind LIKE does not copy.
        let mut create = format!("CREATE {persistence}TABLE {to} (LIKE {from} INCLUDING ALL)");
        if let Some(options) = optional(table, 3).filter(|s| !s.is_empty()) {
            create.push_str(&format!(" WITH ({options})"));
        }
        if let Some(tablespace) = optional(table, 2) {
            create.push_str(&format!(" TABLESPACE {}", d.quote_ident(tablespace)));
        }
        let mut native = vec![create];
        for statement in ddl.iter().skip(1) {
            if tokens(statement, d)?.iter().any(|t| t.is("FOREIGN")) {
                native.push(statement.clone());
            }
        }
        ddl = native;
    }
    Ok(TableDefinition { ddl, columns })
}

#[derive(Debug, Default)]
pub struct InsertOptions {
    pub before: Vec<String>,
    pub after: Vec<String>,
    pub overriding_system_value: bool,
}

fn postgres_sequence_queries(name: &str) -> (String, String) {
    let table = lit(Driver::Postgres, name);
    (
        format!(
            "SELECT a.attname,a.attidentity,pg_get_serial_sequence({table},a.attname),seq.seqincrement,seq.seqcycle FROM pg_attribute a JOIN pg_sequence seq ON seq.seqrelid=pg_get_serial_sequence({table},a.attname)::regclass WHERE a.attrelid=to_regclass({table}) AND a.attnum>0 AND NOT a.attisdropped"
        ),
        format!(
            "SELECT a.attname FROM pg_attribute a JOIN pg_attrdef ad ON ad.adrelid=a.attrelid AND ad.adnum=a.attnum WHERE a.attrelid=to_regclass({table}) AND a.attnum>0 AND NOT a.attisdropped AND a.attidentity='' AND pg_get_expr(ad.adbin,ad.adrelid) LIKE '%nextval(%' AND pg_get_serial_sequence({table},a.attname) IS NULL"
        ),
    )
}

fn postgres_sequence_options(
    name: &str,
    row: &[Option<String>],
    state: &[Option<String>],
) -> anyhow::Result<InsertOptions> {
    anyhow::ensure!(
        optional(row, 2).is_some(),
        trf(
            "Faithful PostgreSQL definition requires a dedicated backup: {reason}",
            &[("reason", &fidelity_reason("shared sequence defaults"))]
        )
    );
    anyhow::ensure!(
        !truth(cell(row, 4)?),
        tr("Cyclic sequences require a dedicated backup")
    );
    let (aggregate, choice, comparison) = if number(cell(row, 3)?)?.parse::<i128>()? > 0 {
        ("MAX", "GREATEST", ">")
    } else {
        ("MIN", "LEAST", "<")
    };
    let last = number(cell(state, 0)?)?;
    let extreme = format!(
        "(SELECT {aggregate}({})::bigint FROM {name})",
        Driver::Postgres.quote_ident(cell(row, 0)?)
    );
    let restore = format!(
        "SELECT pg_catalog.setval(pg_get_serial_sequence({},{}), {choice}({last},COALESCE({extreme},{last})), {} OR COALESCE({extreme} {comparison} {last},false))",
        lit(Driver::Postgres, name),
        lit(Driver::Postgres, cell(row, 0)?),
        if truth(cell(state, 1)?) {
            "true"
        } else {
            "false"
        }
    );
    Ok(InsertOptions {
        before: Vec::new(),
        after: vec![restore],
        overriding_system_value: cell(row, 1)? == "a",
    })
}

/// Identity handling for native INSERT statements and lossless SQL dumps.
pub async fn data_insert_options(
    session: &mut Session,
    driver: Driver,
    schema: &str,
    table: &str,
) -> anyhow::Result<InsertOptions> {
    let name = qualified(driver, schema, table);
    let mut options = InsertOptions::default();
    if driver == Driver::MsSql {
        let rows = query(
            session,
            &format!(
                "SELECT name,CONVERT(varchar(100),last_value),CONVERT(varchar(100),increment_value) FROM sys.identity_columns WHERE object_id=OBJECT_ID({})",
                lit(driver, &name)
            ),
        )
        .await?;
        if !rows.rows.is_empty() {
            options
                .before
                .push(format!("SET IDENTITY_INSERT {name} ON"));
            options
                .after
                .push(format!("SET IDENTITY_INSERT {name} OFF"));
            if let Some(row) = rows.rows.first().filter(|r| optional(r, 1).is_some()) {
                let last = number(cell(row, 1)?)?;
                let increment = number(cell(row, 2)?)?;
                let (aggregate, comparison) = if increment.parse::<i128>()? > 0 {
                    ("MAX", ">")
                } else {
                    ("MIN", "<")
                };
                let column = driver.quote_ident(cell(row, 0)?);
                // An empty freshly-created table uses the reseed value itself on its next INSERT.
                // Rows copied after the preview can also advance beyond its captured counter.
                options.after.push(format!("DECLARE @datacraft_identity decimal(38,0)={last}; SELECT @datacraft_identity=CASE WHEN {aggregate}({column}) {comparison} @datacraft_identity THEN {aggregate}({column}) ELSE @datacraft_identity END FROM {name}; IF NOT EXISTS(SELECT 1 FROM {name}) SET @datacraft_identity=@datacraft_identity+({increment}); DBCC CHECKIDENT ({}, RESEED, @datacraft_identity) WITH NO_INFOMSGS",lit(driver,&name)));
            }
        }
    } else if driver == Driver::Postgres {
        let (owned, shared) = postgres_sequence_queries(&name);
        anyhow::ensure!(
            query(session, &shared).await?.rows.is_empty(),
            trf(
                "Faithful PostgreSQL definition requires a dedicated backup: {reason}",
                &[("reason", &fidelity_reason("shared sequence defaults"))]
            )
        );
        let rows = query(session, &owned).await?;
        for row in rows.rows {
            // The name comes from pg_get_serial_sequence (already identifier-quoted by PostgreSQL).
            let seq = cell(&row, 2)?;
            let state = query(session, &format!("SELECT last_value,is_called FROM {seq}")).await?;
            let state = state
                .rows
                .first()
                .ok_or_else(|| anyhow::anyhow!(tr("Missing identity sequence state")))?;
            let sequence = postgres_sequence_options(&name, &row, state)?;
            options.overriding_system_value |= sequence.overriding_system_value;
            options.after.extend(sequence.after);
        }
    } else if matches!(driver, Driver::MySql | Driver::MariaDb) {
        options
            .before
            .push("SET @datacraft_saved_sql_mode=@@SESSION.sql_mode".into());
        options.before.push(
            "SET SESSION sql_mode=CONCAT_WS(',',@@SESSION.sql_mode,'NO_AUTO_VALUE_ON_ZERO')".into(),
        );
        options
            .after
            .push("SET SESSION sql_mode=@datacraft_saved_sql_mode".into());
    }
    Ok(options)
}

fn retarget_option(sql: &str, driver: Driver, schema: &str, source: &str, target: &str) -> String {
    let from = qualified(driver, schema, source);
    let to = qualified(driver, schema, target);
    // Only generated option statements pass through here, never catalog/user SQL.
    sql.replace(&lit(driver, &from), &lit(driver, &to))
        .replace(&from, &to)
}

#[derive(Debug)]
struct Token {
    start: usize,
    end: usize,
    value: String,
    word: bool,
}

impl Token {
    fn is(&self, keyword: &str) -> bool {
        self.word && self.value.eq_ignore_ascii_case(keyword)
    }
}

/// Token positions keep literals, quoted column names and comments byte-for-byte intact.
fn tokens(sql: &str, driver: Driver) -> anyhow::Result<Vec<Token>> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(b"--") {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes[i..].starts_with(b"/*") {
            i += 2;
            let mut depth = 1;
            while i < bytes.len() && depth > 0 {
                if bytes[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if bytes[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            anyhow::ensure!(
                depth == 0,
                tr("Unterminated SQL comment in table definition")
            );
            continue;
        }
        let start = i;
        let (value, word) = if matches!(bytes[i], b'\'' | b'"' | b'`' | b'[') {
            let quote = bytes[i];
            let close = if quote == b'[' { b']' } else { quote };
            i += 1;
            let mut raw = Vec::new();
            loop {
                anyhow::ensure!(
                    i < bytes.len(),
                    tr("Unterminated SQL quote in table definition")
                );
                if bytes[i] == close {
                    i += 1;
                    if bytes.get(i) == Some(&close) {
                        raw.push(close);
                        i += 1;
                    } else {
                        break;
                    }
                } else if bytes[i] == b'\\'
                    && matches!(quote, b'\'' | b'`')
                    && (matches!(
                        driver,
                        Driver::MySql | Driver::MariaDb | Driver::ClickHouse | Driver::Snowflake
                    ) || (driver == Driver::Postgres
                        && quote == b'\''
                        && start > 0
                        && matches!(bytes[start - 1], b'E' | b'e')))
                {
                    // MySQL catalog DDL escapes both quote characters and backslashes.
                    i += 1;
                    anyhow::ensure!(
                        i < bytes.len(),
                        tr("Incomplete SQL escape in table definition")
                    );
                    raw.push(bytes[i]);
                    i += 1;
                } else {
                    raw.push(bytes[i]);
                    i += 1;
                }
            }
            (String::from_utf8(raw)?, false)
        } else if bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] >= 128 {
            i += 1;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric()
                    || matches!(bytes[i], b'_' | b'$')
                    || bytes[i] >= 128)
            {
                i += 1;
            }
            (sql[start..i].to_string(), true)
        } else {
            i += 1;
            (sql[start..i].to_string(), false)
        };
        out.push(Token {
            start,
            end: i,
            value,
            word,
        });
    }
    Ok(out)
}

fn object_name(ts: &[Token], i: usize) -> anyhow::Result<(usize, Option<&str>, &str)> {
    let first = ts
        .get(i)
        .ok_or_else(|| anyhow::anyhow!(tr("Missing object name in table definition")))?;
    if ts.get(i + 1).is_some_and(|t| t.value == ".") {
        anyhow::ensure!(
            !ts.get(i + 3).is_some_and(|t| t.value == "."),
            tr("Cross-database table definitions require a dedicated backup")
        );
        let name = ts
            .get(i + 2)
            .ok_or_else(|| anyhow::anyhow!(tr("Incomplete qualified table name")))?;
        Ok((i + 2, Some(&first.value), &name.value))
    } else {
        Ok((i, None, &first.value))
    }
}

fn qualified(driver: Driver, schema: &str, table: &str) -> String {
    format!(
        "{}.{}",
        driver.quote_ident(schema),
        driver.quote_ident(table)
    )
}

fn distinct_name(target: &str, original: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    (target, original).hash(&mut hash);
    format!("dc_{:016x}", hash.finish())
}

fn apply_replacements(sql: &str, mut edits: Vec<(usize, usize, String)>) -> anyhow::Result<String> {
    edits.sort_by_key(|e| e.0);
    let mut result = String::new();
    let mut last = 0;
    for (start, end, replacement) in edits {
        anyhow::ensure!(start >= last, tr("Overlapping table definition edits"));
        result.push_str(&sql[last..start]);
        result.push_str(&replacement);
        last = end;
    }
    result.push_str(&sql[last..]);
    Ok(result)
}

fn reference_edits(
    ts: &[Token],
    driver: Driver,
    schema: &str,
    source: &str,
    target: &str,
) -> anyhow::Result<Vec<(usize, usize, String)>> {
    let mut edits = Vec::new();
    for (i, token) in ts.iter().enumerate().filter(|(_, t)| t.is("REFERENCES")) {
        let (end, sch, name) = object_name(ts, i + 1)?;
        let same =
            |a: &str, b: &str| a == b || (driver == Driver::Sqlite && a.eq_ignore_ascii_case(b));
        if same(name, source) && sch.is_none_or(|s| same(s, schema)) {
            let new = if sch.is_some() {
                qualified(driver, schema, target)
            } else {
                driver.quote_ident(target)
            };
            edits.push((token.end.max(ts[i + 1].start), ts[end].end, new));
        }
    }
    Ok(edits)
}

fn rewrite_create(
    sql: &str,
    driver: Driver,
    schema: &str,
    source: &str,
    target: &str,
    index: Option<&str>,
) -> anyhow::Result<String> {
    let ts = tokens(sql, driver)?;
    anyhow::ensure!(
        ts.first().is_some_and(|t| t.is("CREATE")),
        tr("Expected catalog CREATE statement")
    );
    let object = if index.is_some() { "INDEX" } else { "TABLE" };
    let keyword = ts
        .iter()
        .position(|t| t.is(object))
        .ok_or_else(|| anyhow::anyhow!(trf("Expected CREATE {object}", &[("object", &object)])))?;
    let mut pos = keyword + 1;
    let mut edits = Vec::new();
    if ts.get(pos).is_some_and(|t| t.is("IF")) {
        anyhow::ensure!(
            ts.get(pos + 1).is_some_and(|t| t.is("NOT"))
                && ts.get(pos + 2).is_some_and(|t| t.is("EXISTS")),
            tr("Invalid CREATE condition")
        );
        edits.push((ts[pos].start, ts[pos + 2].end, String::new()));
        pos += 3;
    }
    let (end, _, _) = object_name(&ts, pos)?;
    edits.push((
        ts[pos].start,
        ts[end].end,
        qualified(driver, schema, index.unwrap_or(target)),
    ));
    if index.is_some() {
        let on = ts
            .iter()
            .enumerate()
            .skip(end + 1)
            .find(|(_, t)| t.is("ON"))
            .map(|(i, _)| i)
            .ok_or_else(|| anyhow::anyhow!(tr("Missing index table")))?;
        let mut start = on + 1;
        if ts.get(start).is_some_and(|t| t.is("ONLY")) {
            start += 1;
        }
        let (end, _, _) = object_name(&ts, start)?;
        let table = if driver == Driver::Sqlite {
            driver.quote_ident(target)
        } else {
            qualified(driver, schema, target)
        };
        edits.push((ts[start].start, ts[end].end, table));
    } else {
        edits.extend(reference_edits(&ts, driver, schema, source, target)?);
        if matches!(driver, Driver::MySql | Driver::MariaDb) && source != target {
            for (i, _) in ts.iter().enumerate().filter(|(_, t)| t.is("CONSTRAINT")) {
                let name = ts
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!(tr("Missing constraint name")))?;
                if !name.is("FOREIGN")
                    && !name.is("CHECK")
                    && !name.is("PRIMARY")
                    && !name.is("UNIQUE")
                {
                    edits.push((
                        name.start,
                        name.end,
                        driver.quote_ident(&distinct_name(target, &name.value)),
                    ));
                }
            }
        }
    }
    apply_replacements(sql, edits)
}

fn cell(row: &[Option<String>], i: usize) -> anyhow::Result<&str> {
    row.get(i).and_then(Option::as_deref).ok_or_else(|| {
        anyhow::anyhow!(trf(
            "Missing table catalog field {index}",
            &[("index", &i.to_string())]
        ))
    })
}

fn optional(row: &[Option<String>], i: usize) -> Option<&str> {
    row.get(i).and_then(Option::as_deref)
}

fn truth(value: &str) -> bool {
    matches!(value, "1" | "t" | "true" | "TRUE" | "YES")
}

fn number(value: &str) -> anyhow::Result<&str> {
    value
        .parse::<i128>()
        .map_err(|_| anyhow::anyhow!(tr("Invalid numeric table catalog value")))?;
    Ok(value)
}

fn mssql_column(
    row: &[Option<String>],
    default_name: Option<&str>,
) -> anyhow::Result<(String, bool)> {
    let name = Driver::MsSql.quote_ident(cell(row, 0)?);
    if let Some(expr) = optional(row, 10) {
        let persisted = if truth(cell(row, 11)?) {
            if truth(cell(row, 5)?) {
                " PERSISTED"
            } else {
                " PERSISTED NOT NULL"
            }
        } else {
            ""
        };
        return Ok((format!("{name} AS {expr}{persisted}"), false));
    }
    let ty = cell(row, 1)?;
    let suffix = match ty {
        "varchar" | "char" | "varbinary" | "binary" | "nvarchar" | "nchar" => {
            let length = number(cell(row, 2)?)?.parse::<i64>()?;
            if length == -1 {
                "(max)".into()
            } else {
                format!(
                    "({})",
                    if matches!(ty, "nvarchar" | "nchar") {
                        length / 2
                    } else {
                        length
                    }
                )
            }
        }
        "decimal" | "numeric" => format!("({},{})", number(cell(row, 3)?)?, number(cell(row, 4)?)?),
        "datetime2" | "datetimeoffset" | "time" => format!("({})", number(cell(row, 4)?)?),
        "float" => format!("({})", number(cell(row, 3)?)?),
        _ => String::new(),
    };
    let mut sql = format!("{name} {ty}{suffix}");
    if let Some(collation) = optional(row, 6) {
        sql.push_str(&format!(
            " COLLATE {}",
            Driver::MsSql.quote_ident(collation)
        ));
    }
    let identity = optional(row, 8).is_some();
    if let Some(seed) = optional(row, 8) {
        sql.push_str(&format!(
            " IDENTITY({},{})",
            number(seed)?,
            number(cell(row, 9)?)?
        ));
    }
    if truth(cell(row, 12)?) {
        sql.push_str(" ROWGUIDCOL");
    }
    sql.push_str(if truth(cell(row, 5)?) {
        " NULL"
    } else {
        " NOT NULL"
    });
    if let Some(default) = optional(row, 7) {
        if let Some(name) = default_name {
            sql.push_str(&format!(" CONSTRAINT {}", Driver::MsSql.quote_ident(name)));
        }
        sql.push_str(&format!(" DEFAULT {default}"));
    }
    Ok((sql, identity))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ConnectionConfig;

    struct Fixture(std::path::PathBuf);
    impl Fixture {
        async fn open(name: &str) -> (Self, Session) {
            let path = std::env::temp_dir().join(format!(
                "datacraft_definition_{}_{name}.db",
                std::process::id()
            ));
            let _ = std::fs::remove_file(&path);
            std::fs::File::create(&path).unwrap();
            let mut cfg = ConnectionConfig::new(1, Driver::Sqlite);
            cfg.file_path = path.to_string_lossy().into_owned();
            let session = Session::connect(&cfg).await.unwrap();
            (Self(path), session)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    async fn scalar(s: &mut Session, sql: &str) -> Option<String> {
        s.execute(sql, 10).await.unwrap()[0].rows[0][0].clone()
    }

    #[tokio::test]
    async fn sqlite_duplicate_preserves_schema_binary_defaults_and_identity() {
        let (_fixture, mut s) = Fixture::open("faithful").await;
        s.execute("PRAGMA foreign_keys=ON; CREATE TABLE parent(id INTEGER PRIMARY KEY); INSERT INTO parent VALUES(1); \
            CREATE TABLE \"source\" (id INTEGER PRIMARY KEY AUTOINCREMENT, parent_id INTEGER REFERENCES parent(id), \
            label TEXT NOT NULL DEFAULT 'source', payload BLOB, derived TEXT GENERATED ALWAYS AS (label || '_source') STORED, \
            CONSTRAINT positive CHECK(id > 0), UNIQUE(label)); \
            CREATE INDEX \"source_idx\" ON source(lower(label)) WHERE label <> 'source'; \
            INSERT INTO source(id,parent_id,label,payload) VALUES(1,1,'first',zeroblob(512)); \
            INSERT INTO source(id,label) VALUES(900,'deleted'); DELETE FROM source WHERE id=900", 0).await.unwrap();
        let def = definition(&mut s, Driver::Sqlite, "main", "source")
            .await
            .unwrap();
        assert_eq!(def.columns, ["id", "parent_id", "label", "payload"]);
        assert_eq!(def.ddl.len(), 2);
        let plan = duplicate_plan(
            &mut s,
            Driver::Sqlite,
            "main",
            "source",
            "copy\"source",
            true,
        )
        .await
        .unwrap();
        assert!(plan.transaction);
        s.run_transaction(&plan.statements).await.unwrap();
        assert_eq!(scalar(&mut s, "SELECT hex(payload)=hex((SELECT payload FROM source)) AND length(payload)=512 FROM \"copy\"\"source\"").await.as_deref(), Some("1"));
        assert_eq!(
            scalar(&mut s, "SELECT derived FROM \"copy\"\"source\"")
                .await
                .as_deref(),
            Some("first_source")
        );
        s.execute(
            "INSERT INTO \"copy\"\"source\"(parent_id,payload) VALUES(1,X'01')",
            0,
        )
        .await
        .unwrap();
        assert_eq!(
            scalar(
                &mut s,
                "SELECT id FROM \"copy\"\"source\" WHERE label='source'"
            )
            .await
            .as_deref(),
            Some("901")
        );
        assert!(
            s.execute(
                "INSERT INTO \"copy\"\"source\"(parent_id,label) VALUES(999,'bad')",
                0
            )
            .await
            .is_err()
        );
        assert!(
            s.execute("INSERT INTO \"copy\"\"source\"(label) VALUES('first')", 0)
                .await
                .is_err()
        );
        assert_eq!(
            scalar(
                &mut s,
                "SELECT count(*) FROM sqlite_schema WHERE type='index' AND tbl_name='copy\"source'"
            )
            .await
            .as_deref(),
            Some("2")
        );
    }

    #[tokio::test]
    async fn sqlite_structure_only_self_reference_and_definition_restore() {
        let (_fixture, mut s) = Fixture::open("self").await;
        s.execute("CREATE TABLE \"source source\" (id INTEGER PRIMARY KEY, parent INT REFERENCES \"source source\"(id), \
            \"source source\" TEXT DEFAULT 'source source'); INSERT INTO \"source source\"(id) VALUES(1); \
            CREATE INDEX idx ON \"source source\"(\"source source\")",0).await.unwrap();
        let def = definition(&mut s, Driver::Sqlite, "main", "source source")
            .await
            .unwrap();
        let plan = duplicate_plan(
            &mut s,
            Driver::Sqlite,
            "main",
            "source source",
            "target",
            false,
        )
        .await
        .unwrap();
        s.run_transaction(&plan.statements).await.unwrap();
        assert_eq!(
            scalar(&mut s, "SELECT count(*) FROM target")
                .await
                .as_deref(),
            Some("0")
        );
        assert_eq!(
            scalar(
                &mut s,
                "SELECT \"table\" FROM pragma_foreign_key_list('target')"
            )
            .await
            .as_deref(),
            Some("target")
        );
        assert_eq!(
            scalar(
                &mut s,
                "SELECT dflt_value FROM pragma_table_info('target') WHERE name='source source'"
            )
            .await
            .as_deref(),
            Some("'source source'")
        );
        s.execute("DROP TABLE \"source source\"", 0).await.unwrap();
        s.run_transaction(&def.ddl).await.unwrap();
        assert_eq!(
            scalar(
                &mut s,
                "SELECT count(*) FROM pragma_index_list('source source')"
            )
            .await
            .as_deref(),
            Some("1")
        );
    }

    #[tokio::test]
    async fn sqlite_conflicts_and_failed_plans_do_not_damage_existing_tables() {
        let (_fixture, mut s) = Fixture::open("rollback").await;
        s.execute("CREATE TABLE source(id INTEGER PRIMARY KEY); INSERT INTO source VALUES(42); CREATE TABLE existing(id INT)",0).await.unwrap();
        assert!(
            duplicate_plan(&mut s, Driver::Sqlite, "main", "source", "source", true)
                .await
                .is_err()
        );
        assert!(
            duplicate_plan(&mut s, Driver::Sqlite, "main", "source", "existing", true)
                .await
                .is_err()
        );
        let mut plan = duplicate_plan(&mut s, Driver::Sqlite, "main", "source", "target", true)
            .await
            .unwrap();
        plan.statements.push("INSERT INTO target VALUES(42)".into());
        assert!(s.run_transaction(&plan.statements).await.is_err());
        assert_eq!(
            scalar(
                &mut s,
                "SELECT count(*) FROM sqlite_schema WHERE name='target'"
            )
            .await
            .as_deref(),
            Some("0")
        );
        assert_eq!(
            scalar(&mut s, "SELECT id FROM source").await.as_deref(),
            Some("42")
        );
    }

    #[tokio::test]
    async fn sqlite_rejects_trigger_and_virtual_table_fidelity_gaps() {
        let (_fixture, mut s) = Fixture::open("unsupported").await;
        s.execute("CREATE TABLE source(id INT); CREATE TRIGGER trigger_source AFTER INSERT ON source BEGIN UPDATE source SET id=id+1; END",0).await.unwrap();
        assert!(
            definition(&mut s, Driver::Sqlite, "main", "source")
                .await
                .is_err()
        );
        assert!(
            duplicate_plan(&mut s, Driver::Sqlite, "main", "source", "target", true)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn sqlite_preserves_observable_rowids_and_attached_schema_isolation() {
        let (_fixture, mut s) = Fixture::open("rowids").await;
        s.execute("ATTACH ':memory:' AS other; CREATE TABLE main.source(v TEXT); CREATE TABLE other.source(v TEXT); INSERT INTO main.source(rowid,v) VALUES(99,'main'); INSERT INTO other.source(rowid,v) VALUES(700,'attached')",0).await.unwrap();
        let plan = duplicate_plan(&mut s, Driver::Sqlite, "other", "source", "target", true)
            .await
            .unwrap();
        s.run_transaction(&plan.statements).await.unwrap();
        assert_eq!(
            scalar(&mut s, "SELECT rowid || ':' || v FROM other.target")
                .await
                .as_deref(),
            Some("700:attached")
        );
        assert_eq!(
            scalar(
                &mut s,
                "SELECT count(*) FROM main.sqlite_schema WHERE name='target'"
            )
            .await
            .as_deref(),
            Some("0")
        );
    }

    #[test]
    fn postgres_escape_strings_and_case_sensitive_references_are_preserved() {
        let sql = "CREATE TABLE source (note text DEFAULT E'can\\'t REFERENCES source', id int REFERENCES source(id), other int REFERENCES \"Source\"(id))";
        let rewritten =
            rewrite_create(sql, Driver::Postgres, "public", "source", "target", None).unwrap();
        assert!(rewritten.contains("E'can\\'t REFERENCES source'"));
        assert!(rewritten.contains("id int REFERENCES \"target\"(id)"));
        assert!(rewritten.contains("other int REFERENCES \"Source\"(id)"));
    }

    #[test]
    fn postgres_catalog_literals_escape_backslashes_explicitly() {
        assert_eq!(
            lit(Driver::Postgres, "schema\\'; SELECT 1; --"),
            "E'schema\\\\''; SELECT 1; --'"
        );
    }

    #[test]
    fn postgres_owned_serial_values_restore_sequence_without_identity_override() {
        let state = [Some("900".to_string()), Some("t".to_string())];
        for (identity, overriding) in [("", false), ("d", false), ("a", true)] {
            let row = [
                Some("id"),
                Some(identity),
                Some("public.source_id_seq"),
                Some("1"),
                Some("f"),
            ]
            .map(|s| s.map(str::to_string));
            let options = postgres_sequence_options("\"public\".\"source\"", &row, &state).unwrap();
            assert_eq!(options.overriding_system_value, overriding);
            assert_eq!(
                options.after,
                [
                    "SELECT pg_catalog.setval(pg_get_serial_sequence(E'\"public\".\"source\"',E'id'), GREATEST(900,COALESCE((SELECT MAX(\"id\")::bigint FROM \"public\".\"source\"),900)), true OR COALESCE((SELECT MAX(\"id\")::bigint FROM \"public\".\"source\") > 900,false))"
                ]
            );
        }
        let shared =
            [Some("id"), Some(""), None, Some("1"), Some("f")].map(|s| s.map(str::to_string));
        assert!(postgres_sequence_options("\"public\".\"source\"", &shared, &state).is_err());
    }

    #[test]
    fn postgres_sequence_queries_cover_owned_serials_and_reject_shared_defaults() {
        let (owned, shared) = postgres_sequence_queries("\"public\".\"source\"");
        assert_eq!(
            owned,
            "SELECT a.attname,a.attidentity,pg_get_serial_sequence(E'\"public\".\"source\"',a.attname),seq.seqincrement,seq.seqcycle FROM pg_attribute a JOIN pg_sequence seq ON seq.seqrelid=pg_get_serial_sequence(E'\"public\".\"source\"',a.attname)::regclass WHERE a.attrelid=to_regclass(E'\"public\".\"source\"') AND a.attnum>0 AND NOT a.attisdropped"
        );
        assert_eq!(
            shared,
            "SELECT a.attname FROM pg_attribute a JOIN pg_attrdef ad ON ad.adrelid=a.attrelid AND ad.adnum=a.attnum WHERE a.attrelid=to_regclass(E'\"public\".\"source\"') AND a.attnum>0 AND NOT a.attisdropped AND a.attidentity='' AND pg_get_expr(ad.adbin,ad.adrelid) LIKE '%nextval(%' AND pg_get_serial_sequence(E'\"public\".\"source\"',a.attname) IS NULL"
        );
    }

    #[test]
    fn cross_database_definitions_are_rejected_instead_of_misretargeted() {
        assert!(
            rewrite_create(
                "CREATE TABLE db.schema.source (id int)",
                Driver::Snowflake,
                "schema",
                "source",
                "target",
                None
            )
            .is_err()
        );
    }

    #[test]
    fn snowflake_native_clone_rejects_source_modifiers_that_would_be_lost() {
        for modifier in [
            "TRANSIENT",
            "TEMPORARY",
            "TEMP",
            "HYBRID",
            "ICEBERG",
            "DYNAMIC",
            "EXTERNAL",
            "EVENT",
        ] {
            let sql = format!("CREATE OR REPLACE {modifier} TABLE source (id NUMBER NOT NULL)");
            assert!(validate_snowflake_source(&sql, true).is_err(), "{modifier}");
        }
    }

    #[test]
    fn snowflake_definition_can_preserve_transient_header_and_ordinary_defaults() {
        for sql in [
            "CREATE OR REPLACE TABLE source (id NUMBER IDENTITY(10,2), text VARCHAR DEFAULT 'AS (value)', value NUMBER DEFAULT CAST(1 AS NUMBER), \"AS\" VARCHAR)",
            "CREATE /* header */ TABLE \"db\".\"source\" (payload BINARY, id NUMBER, CONSTRAINT fk FOREIGN KEY (id) REFERENCES parent(id))",
        ] {
            validate_snowflake_source(sql, true).unwrap();
        }
        let sql =
            "CREATE OR REPLACE TRANSIENT TABLE source (id NUMBER) DATA_RETENTION_TIME_IN_DAYS=0";
        validate_snowflake_source(sql, false).unwrap();
        let rewritten = rewrite_create(
            "CREATE TRANSIENT TABLE source (id NUMBER) DATA_RETENTION_TIME_IN_DAYS=0",
            Driver::Snowflake,
            "schema",
            "source",
            "source",
            None,
        )
        .unwrap();
        assert!(rewritten.starts_with("CREATE TRANSIENT TABLE \"schema\".\"source\""));
        assert!(rewritten.ends_with("DATA_RETENTION_TIME_IN_DAYS=0"));
    }

    #[test]
    fn snowflake_virtual_columns_are_rejected_before_export_or_native_clone() {
        for column in [
            "total NUMBER AS (id+1)",
            "total NUMBER GENERATED ALWAYS AS (id+1) VIRTUAL",
        ] {
            let sql = format!("CREATE TABLE source (id NUMBER, {column})");
            assert!(validate_snowflake_source(&sql, false).is_err());
            assert!(validate_snowflake_source(&sql, true).is_err());
        }
    }

    #[test]
    fn sql_dump_creates_separate_constraints_after_data_in_original_order() {
        for d in [Driver::Postgres, Driver::MsSql] {
            let ddl=vec!["CREATE TABLE source(id INT, parent INT)".into(),"ALTER TABLE source ADD CONSTRAINT pk PRIMARY KEY(id)".into(),"ALTER TABLE source WITH NOCHECK ADD CONSTRAINT fk FOREIGN KEY(parent) REFERENCES source(id)".into(),"ALTER TABLE source NOCHECK CONSTRAINT fk".into()];
            let (before, after) = dump_ddl(d, "schema", "source", &ddl).unwrap();
            assert_eq!(before, ["CREATE TABLE source(id INT, parent INT)"]);
            assert_eq!(
                after,
                [
                    "ALTER TABLE source ADD CONSTRAINT pk PRIMARY KEY(id)",
                    "ALTER TABLE source WITH NOCHECK ADD CONSTRAINT fk FOREIGN KEY(parent) REFERENCES source(id)",
                    "ALTER TABLE source NOCHECK CONSTRAINT fk"
                ]
            );
        }
    }

    #[test]
    fn mysql_dump_extracts_whole_foreign_key_clause_without_touching_defaults() {
        let sql = "CREATE TABLE `db`.`source` (`id` int NOT NULL, `parent` int DEFAULT NULL, `label` varchar(100) DEFAULT 'a, REFERENCES source', CONSTRAINT `fk``parent` FOREIGN KEY (`parent`) REFERENCES `source` (`id`) ON DELETE CASCADE ON UPDATE SET NULL, PRIMARY KEY (`id`)) ENGINE=InnoDB COMMENT='a,b'";
        for d in [Driver::MySql, Driver::MariaDb] {
            let (before, after) = dump_ddl(d, "db", "source", &[sql.into()]).unwrap();
            assert_eq!(
                before,
                [
                    "CREATE TABLE `db`.`source` (`id` int NOT NULL, `parent` int DEFAULT NULL, `label` varchar(100) DEFAULT 'a, REFERENCES source', PRIMARY KEY (`id`)) ENGINE=InnoDB COMMENT='a,b'"
                ]
            );
            assert_eq!(
                after,
                [
                    "ALTER TABLE `db`.`source` ADD CONSTRAINT `fk``parent` FOREIGN KEY (`parent`) REFERENCES `source` (`id`) ON DELETE CASCADE ON UPDATE SET NULL"
                ]
            );
        }
        assert!(
            dump_ddl(
                Driver::MySql,
                "db",
                "source",
                &["CREATE TABLE source(id INT, parent INT REFERENCES source(id))".into()]
            )
            .is_err()
        );
    }

    #[test]
    fn mysql_trigger_catalog_visibility_requires_a_matching_direct_grant() {
        for grant in [
            "GRANT TRIGGER ON *.* TO 'user'@'host'",
            "GRANT ALL PRIVILEGES ON `db`.* TO 'user'@'host'",
            "GRANT SELECT, TRIGGER ON `db`.`source` TO 'user'@'host'",
        ] {
            assert!(mysql_trigger_visible(&[grant.into()], "db", "source").unwrap());
        }
        for grants in [
            vec!["GRANT SELECT ON `db`.`source` TO 'user'@'host'".into()],
            vec!["GRANT TRIGGER ON `other`.* TO 'user'@'host'".into()],
            vec!["GRANT TRIGGER ON `db`.`other` TO 'user'@'host'".into()],
            vec!["GRANT `trigger_role` TO 'user'@'host'".into()],
            vec![
                "GRANT ALL PRIVILEGES ON *.* TO 'user'@'host'".into(),
                "REVOKE TRIGGER ON `db`.* FROM 'user'@'host'".into(),
            ],
        ] {
            assert!(!mysql_trigger_visible(&grants, "db", "source").unwrap());
        }
    }

    #[test]
    fn mysql_trigger_visibility_does_not_confuse_quoted_stars_or_column_names_with_privileges() {
        for grant in [
            "GRANT TRIGGER ON `*`.* TO 'user'@'host'",
            "GRANT TRIGGER ON `db`.`*` TO 'user'@'host'",
            "GRANT SELECT (TRIGGER) ON `db`.`source` TO 'user'@'host'",
        ] {
            assert!(
                !mysql_trigger_visible(&[grant.into()], "db", "source").unwrap(),
                "{grant}"
            );
        }
    }

    #[test]
    fn mssql_named_default_cannot_modify_keyword_text_inside_a_literal() {
        let row = [
            Some("label"),
            Some("varchar"),
            Some("100"),
            Some("0"),
            Some("0"),
            Some("1"),
            None,
            Some("(' DEFAULT ')"),
            None,
            None,
            None,
            Some("0"),
            Some("0"),
        ]
        .map(|s| s.map(str::to_owned));
        assert_eq!(
            mssql_column(&row, Some("DF label")).unwrap().0,
            "[label] varchar(100) NULL CONSTRAINT [DF label] DEFAULT (' DEFAULT ')"
        );
    }

    #[test]
    fn mssql_persisted_not_null_constraint_is_preserved() {
        let row = [
            Some("total"),
            Some("int"),
            Some("4"),
            Some("10"),
            Some("0"),
            Some("0"),
            None,
            None,
            None,
            None,
            Some("([id]+1)"),
            Some("1"),
            Some("0"),
        ]
        .map(|s| s.map(str::to_owned));
        assert_eq!(
            mssql_column(&row, None).unwrap().0,
            "[total] AS ([id]+1) PERSISTED NOT NULL"
        );
    }

    #[test]
    fn mysql_rewrites_only_object_positions_and_schema_scoped_constraints() {
        let sql = "CREATE TABLE `source` (`source` int DEFAULT 1, label text DEFAULT 'source', CONSTRAINT `source_fk` FOREIGN KEY (`source`) REFERENCES `source` (`source`)) ENGINE=InnoDB";
        let rewritten =
            rewrite_create(sql, Driver::MySql, "db", "source", "copy`table", None).unwrap();
        assert!(rewritten.starts_with("CREATE TABLE `db`.`copy``table` ("));
        assert!(rewritten.contains("`source` int DEFAULT 1"));
        assert!(rewritten.contains("DEFAULT 'source'"));
        assert!(rewritten.contains("REFERENCES `copy``table` (`source`)"));
        assert!(!rewritten.contains("CONSTRAINT `source_fk`"));
    }

    #[test]
    fn sqlite_comments_quoted_keywords_and_literals_are_not_rewritten() {
        let sql = "CREATE /* TABLE misleading */ TABLE [source] ([REFERENCES] TEXT DEFAULT 'REFERENCES source', id INT REFERENCES /* note */ [source](id))";
        let rewritten =
            rewrite_create(sql, Driver::Sqlite, "main", "source", "target", None).unwrap();
        assert!(rewritten.contains("TABLE \"main\".\"target\""));
        assert!(rewritten.contains("[REFERENCES] TEXT DEFAULT 'REFERENCES source'"));
        assert!(rewritten.contains("REFERENCES /* note */ \"target\"(id)"));
    }

    #[test]
    fn sqlite_backslashes_are_literal_and_mysql_escaped_names_are_decoded() {
        let sql =
            "CREATE TABLE source (path TEXT DEFAULT 'folder\\', id INT REFERENCES source(id))";
        let rewritten =
            rewrite_create(sql, Driver::Sqlite, "main", "source", "target", None).unwrap();
        assert!(rewritten.contains("DEFAULT 'folder\\'"));
        let sql = "CREATE TABLE `so\\`urce` (id INT REFERENCES `so\\`urce`(id))";
        let rewritten =
            rewrite_create(sql, Driver::MySql, "db", "so`urce", "target", None).unwrap();
        assert!(rewritten.contains("REFERENCES `target`(id)"));
    }

    #[test]
    fn mssql_preserves_identity_default_binary_and_computed_column_attributes() {
        let row = |items: &[Option<&str>]| {
            items
                .iter()
                .map(|s| s.map(str::to_owned))
                .collect::<Vec<_>>()
        };
        let id = row(&[
            Some("id]"),
            Some("int"),
            Some("4"),
            Some("10"),
            Some("0"),
            Some("0"),
            None,
            None,
            Some("10"),
            Some("2"),
            None,
            Some("0"),
            Some("0"),
        ]);
        assert_eq!(
            mssql_column(&id, None).unwrap(),
            ("[id]]] int IDENTITY(10,2) NOT NULL".into(), true)
        );
        let blob = row(&[
            Some("payload"),
            Some("varbinary"),
            Some("-1"),
            Some("0"),
            Some("0"),
            Some("1"),
            None,
            Some("(0x00FF)"),
            None,
            None,
            None,
            Some("0"),
            Some("0"),
        ]);
        assert_eq!(
            mssql_column(&blob, None).unwrap(),
            (
                "[payload] varbinary(max) NULL DEFAULT (0x00FF)".into(),
                false
            )
        );
        let computed = row(&[
            Some("total"),
            Some("int"),
            Some("4"),
            Some("10"),
            Some("0"),
            Some("1"),
            None,
            None,
            None,
            None,
            Some("([id]+1)"),
            Some("1"),
            Some("0"),
        ]);
        assert_eq!(
            mssql_column(&computed, None).unwrap(),
            ("[total] AS ([id]+1) PERSISTED".into(), false)
        );
    }
}
