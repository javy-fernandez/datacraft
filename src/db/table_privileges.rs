//! Explicit grants on one table, excluding inherited/effective permissions.

use super::{Driver, ResultSet, Session};
use crate::i18n::tr;
use anyhow::Context;
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    pub name: String,
    pub host: String,
}

#[derive(Clone, Debug)]
pub struct Grants {
    pub principals: Vec<Principal>,
    pub privileges: Vec<String>,
    pub granted: Vec<String>,
}

pub fn supported(driver: Driver) -> bool {
    !matches!(
        driver,
        Driver::Sqlite | Driver::Redis | Driver::MongoDb | Driver::InfluxDb | Driver::Kusto
    )
}

fn privileges(driver: Driver) -> &'static [&'static str] {
    match driver {
        Driver::Postgres => &[
            "SELECT",
            "INSERT",
            "UPDATE",
            "DELETE",
            "TRUNCATE",
            "REFERENCES",
            "TRIGGER",
        ],
        Driver::MySql | Driver::MariaDb => &[
            "SELECT",
            "INSERT",
            "UPDATE",
            "DELETE",
            "CREATE",
            "DROP",
            "REFERENCES",
            "INDEX",
            "ALTER",
            "CREATE VIEW",
            "SHOW VIEW",
            "TRIGGER",
        ],
        Driver::MsSql => &[
            "SELECT",
            "INSERT",
            "UPDATE",
            "DELETE",
            "REFERENCES",
            "ALTER",
            "CONTROL",
            "TAKE OWNERSHIP",
            "VIEW DEFINITION",
        ],
        Driver::Oracle => &[
            "SELECT",
            "INSERT",
            "UPDATE",
            "DELETE",
            "REFERENCES",
            "ALTER",
            "INDEX",
            "READ",
            "FLASHBACK",
            "DEBUG",
        ],
        Driver::Snowflake => &[
            "SELECT",
            "INSERT",
            "UPDATE",
            "DELETE",
            "TRUNCATE",
            "REFERENCES",
        ],
        Driver::ClickHouse => &[
            "SELECT",
            "INSERT",
            "ALTER UPDATE",
            "ALTER DELETE",
            "TRUNCATE",
            "OPTIMIZE",
        ],
        _ => &[],
    }
}

fn validate(
    driver: Driver,
    schema: &str,
    table: &str,
    principal: Option<&Principal>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        supported(driver),
        tr("Table grants are not available for this engine")
    );
    anyhow::ensure!(
        !schema.is_empty() && !table.is_empty() && !schema.contains('\0') && !table.contains('\0'),
        tr("Invalid table name")
    );
    if let Some(p) = principal {
        anyhow::ensure!(
            !p.name.contains('\0') && !p.host.contains('\0'),
            tr("Invalid principal")
        );
        if !matches!(driver, Driver::MySql | Driver::MariaDb) {
            anyhow::ensure!(
                !p.name.is_empty() && p.host.is_empty(),
                tr("Invalid principal")
            );
        }
    }
    Ok(())
}

fn rights(driver: Driver, values: &[String]) -> anyhow::Result<BTreeSet<String>> {
    values
        .iter()
        .map(|value| {
            let value = value.trim().to_ascii_uppercase();
            anyhow::ensure!(
                privileges(driver).contains(&value.as_str()),
                tr("Unsupported table privilege")
            );
            Ok(value)
        })
        .collect()
}

/// Only diff a freshly loaded, editable snapshot. Grant options and grantors are never edited.
pub fn statements(
    driver: Driver,
    schema: &str,
    table: &str,
    principal: &Principal,
    old: &[String],
    new: &[String],
) -> anyhow::Result<Vec<String>> {
    validate(driver, schema, table, Some(principal))?;
    let old = rights(driver, old)?;
    let new = rights(driver, new)?;
    let table = Session::quote_table(driver, schema, table);
    let target = match driver {
        Driver::Postgres | Driver::Snowflake => format!("TABLE {table}"),
        Driver::MsSql => format!("OBJECT::{table}"),
        _ => table,
    };
    let who = if matches!(driver, Driver::MySql | Driver::MariaDb) {
        // Identifier quoting is independent of NO_BACKSLASH_ESCAPES and ANSI_QUOTES.
        format!(
            "{}@{}",
            driver.quote_ident(&principal.name),
            driver.quote_ident(&principal.host)
        )
    } else if driver == Driver::Snowflake {
        format!("ROLE {}", driver.quote_ident(&principal.name))
    } else {
        driver.quote_ident(&principal.name)
    };
    let mut result = Vec::new();
    let removed = old.difference(&new).cloned().collect::<Vec<_>>();
    if !removed.is_empty() {
        let restrict = if matches!(driver, Driver::Postgres | Driver::Snowflake) {
            " RESTRICT"
        } else {
            ""
        };
        result.push(format!(
            "REVOKE {} ON {target} FROM {who}{restrict}",
            removed.join(", ")
        ));
    }
    let added = new.difference(&old).cloned().collect::<Vec<_>>();
    if !added.is_empty() {
        result.push(format!("GRANT {} ON {target} TO {who}", added.join(", ")));
    }
    Ok(result)
}

fn field<'a>(rs: &ResultSet, row: &'a [Option<String>], name: &str) -> anyhow::Result<&'a str> {
    let index = rs
        .columns
        .iter()
        .position(|c| c.eq_ignore_ascii_case(name))
        .context(tr("Incomplete table grant metadata"))?;
    row.get(index)
        .and_then(|c| c.as_deref())
        .context(tr("Incomplete table grant metadata"))
}

fn checked_grants(driver: Driver, rs: &ResultSet) -> anyhow::Result<Vec<String>> {
    anyhow::ensure!(!rs.truncated, tr("Table grant metadata was truncated"));
    anyhow::ensure!(
        rs.columns
            .iter()
            .any(|c| c.eq_ignore_ascii_case("privilege"))
            && rs
                .columns
                .iter()
                .any(|c| c.eq_ignore_ascii_case("editable")),
        tr("Incomplete table grant metadata")
    );
    let mut result = BTreeSet::new();
    for row in &rs.rows {
        anyhow::ensure!(
            field(rs, row, "editable")? == "1",
            tr(
                "This principal has grant options, column permissions, ownership, or grants from another grantor; edit these grants in the SQL editor"
            )
        );
        let values = field(rs, row, "privilege")?
            .split(',')
            .filter(|s| !s.trim().is_empty())
            .map(String::from)
            .collect::<Vec<_>>();
        result.extend(rights(driver, &values)?);
    }
    Ok(result.into_iter().collect())
}

async fn query(session: &mut Session, sql: &str) -> anyhow::Result<ResultSet> {
    let result = session
        .execute(sql, 100_000)
        .await?
        .into_iter()
        .next()
        .context(tr("Incomplete table grant metadata"))?;
    anyhow::ensure!(!result.truncated, tr("Table grant metadata was truncated"));
    Ok(result)
}

fn catalog_literal(driver: Driver, value: &str) -> String {
    if matches!(driver, Driver::MySql | Driver::MariaDb) {
        let hex = value
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        format!("CONVERT(X'{hex}' USING utf8mb4)")
    } else if driver == Driver::Postgres {
        format!("E'{}'", value.replace('\\', "\\\\").replace('\'', "''"))
    } else {
        crate::edit::literal(driver, &Some(value.into()))
    }
}

fn principals_sql(driver: Driver) -> &'static str {
    match driver {
        Driver::Postgres => "SELECT rolname AS name, '' AS host FROM pg_roles ORDER BY 1",
        Driver::MySql => {
            "SELECT CONVERT(User USING utf8mb4) AS name, CONVERT(Host USING utf8mb4) AS host FROM mysql.user ORDER BY 1, 2"
        }
        // MariaDB role grants use a different TO grammar from user@host grants.
        Driver::MariaDb => {
            "SELECT CONVERT(User USING utf8mb4) AS name, CONVERT(Host USING utf8mb4) AS host FROM mysql.user WHERE IFNULL(is_role, 'N') <> 'Y' ORDER BY 1, 2"
        }
        Driver::MsSql => {
            "SELECT name, '' AS host FROM sys.database_principals WHERE type IN ('S','U','G','R','E','X') AND name NOT IN ('dbo','guest','INFORMATION_SCHEMA','sys') ORDER BY 1"
        }
        Driver::Oracle => {
            "SELECT username AS name, '' AS host FROM dba_users UNION SELECT role AS name, '' AS host FROM dba_roles ORDER BY 1"
        }
        Driver::Snowflake => "SHOW ROLES",
        // User and role names share ClickHouse's GRANT namespace. Reject collisions below.
        Driver::ClickHouse => {
            "SELECT name, '' AS host FROM system.users UNION ALL SELECT name, '' AS host FROM system.roles ORDER BY name"
        }
        _ => "",
    }
}

fn grants_sql(driver: Driver, schema: &str, table: &str, p: &Principal) -> String {
    let s = catalog_literal(driver, schema);
    let t = catalog_literal(driver, table);
    let name = catalog_literal(driver, &p.name);
    match driver {
        Driver::Postgres => format!(
            "SELECT a.privilege_type AS privilege, CASE WHEN NOT a.is_grantable AND a.grantor = (SELECT oid FROM pg_roles WHERE rolname = CURRENT_USER) AND c.relowner <> a.grantee THEN 1 ELSE 0 END AS editable \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace CROSS JOIN LATERAL aclexplode(c.relacl) a \
             WHERE n.nspname = {s} AND c.relname = {t} AND a.grantee = (SELECT oid FROM pg_roles WHERE rolname = {name}) \
             UNION ALL SELECT 'SELECT', 0 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace JOIN pg_attribute att ON att.attrelid = c.oid CROSS JOIN LATERAL aclexplode(att.attacl) a \
             WHERE n.nspname = {s} AND c.relname = {t} AND a.grantee = (SELECT oid FROM pg_roles WHERE rolname = {name}) \
             UNION ALL SELECT 'SELECT', 0 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = {s} AND c.relname = {t} AND c.relowner = (SELECT oid FROM pg_roles WHERE rolname = {name})"
        ),
        Driver::MySql | Driver::MariaDb => {
            let host = catalog_literal(driver, &p.host);
            format!(
                "SELECT CONVERT(Table_priv USING utf8mb4) AS privilege, CASE WHEN FIND_IN_SET('Grant', Table_priv) = 0 AND Column_priv = '' THEN 1 ELSE 0 END AS editable FROM mysql.tables_priv \
                     WHERE Db = {s} AND Table_name = {t} AND User = {name} AND Host = {host} \
                     UNION ALL SELECT 'SELECT', 0 FROM mysql.columns_priv WHERE Db = {s} AND Table_name = {t} AND User = {name} AND Host = {host}"
            )
        }
        Driver::MsSql => format!(
            "SELECT p.permission_name AS privilege, CASE WHEN p.state = 'G' AND p.minor_id = 0 AND p.grantor_principal_id = USER_ID() THEN 1 ELSE 0 END AS editable \
             FROM sys.database_permissions p JOIN sys.objects o ON o.object_id = p.major_id JOIN sys.schemas s ON s.schema_id = o.schema_id \
             WHERE p.class = 1 AND s.name = {s} AND o.name = {t} AND p.grantee_principal_id = DATABASE_PRINCIPAL_ID({name})"
        ),
        Driver::Oracle => format!(
            "SELECT privilege, CASE WHEN grantable = 'NO' AND hierarchy = 'NO' AND common = 'NO' AND inherited = 'NO' AND grantor = SYS_CONTEXT('USERENV', 'CURRENT_USER') THEN 1 ELSE 0 END AS editable \
             FROM dba_tab_privs WHERE owner = {s} AND table_name = {t} AND grantee = {name} \
             UNION ALL SELECT privilege, 0 FROM dba_col_privs WHERE owner = {s} AND table_name = {t} AND grantee = {name}"
        ),
        Driver::Snowflake => format!(
            "SHOW GRANTS ON TABLE {}",
            Session::quote_table(driver, schema, table)
        ),
        Driver::ClickHouse => format!(
            "SELECT toString(access_type) AS privilege, if(grant_option = 0 AND is_partial_revoke = 0 AND is_wildcard = 0 AND `column` IS NULL AND database = {s} AND `table` = {t}, 1, 0) AS editable \
             FROM system.grants WHERE (user_name = {name} OR role_name = {name}) AND (is_wildcard = 1 OR ((database = {s} OR database IS NULL) AND (`table` = {t} OR `table` IS NULL)))"
        ),
        _ => String::new(),
    }
}

/// Requires catalog visibility; a missing permission is an error, never an empty grant list.
/// Call again immediately before applying to reject a changed or newly unsafe snapshot.
pub async fn load(
    session: &mut Session,
    driver: Driver,
    schema: &str,
    table: &str,
    principal: Option<&Principal>,
) -> anyhow::Result<Grants> {
    validate(driver, schema, table, principal)?;
    if matches!(driver, Driver::MySql | Driver::MariaDb) {
        // Older MySQL and MariaDB can recreate an account dropped after metadata was loaded.
        let mode = query(
            session,
            "SELECT @@SESSION.sql_mode AS mode, VERSION() AS version",
        )
        .await?;
        let row = mode
            .rows
            .first()
            .context(tr("Incomplete table grant metadata"))?;
        let major = field(&mode, row, "version")?
            .split('.')
            .next()
            .context(tr("Incomplete table grant metadata"))?
            .parse::<u32>()?;
        if driver == Driver::MariaDb || major < 8 {
            anyhow::ensure!(
                field(&mode, row, "mode")?
                    .split(',')
                    .any(|m| m.eq_ignore_ascii_case("NO_AUTO_CREATE_USER")),
                tr("Table grants require NO_AUTO_CREATE_USER in this session SQL mode")
            );
        }
    }
    if driver == Driver::MsSql {
        // Catalog filtering must not turn another grantor's hidden grants into unchecked boxes.
        let visibility = query(
            session,
            "SELECT HAS_PERMS_BY_NAME(DB_NAME(), 'DATABASE', 'VIEW DEFINITION') AS visible",
        )
        .await?;
        let row = visibility
            .rows
            .first()
            .context(tr("Incomplete table grant metadata"))?;
        anyhow::ensure!(
            field(&visibility, row, "visible")? == "1",
            tr("Viewing all table grants requires VIEW DEFINITION on the database")
        );
    }
    let list = query(session, principals_sql(driver)).await.context(tr(
        "Could not read table grant principals; catalog access is required",
    ))?;
    let mut principals = Vec::new();
    for row in &list.rows {
        let name = field(&list, row, "name")?.to_string();
        let host = if driver == Driver::Snowflake {
            String::new()
        } else {
            field(&list, row, "host")?.to_string()
        };
        let p = Principal { name, host };
        anyhow::ensure!(!principals.contains(&p), tr("Ambiguous principal name"));
        principals.push(p);
    }
    let mut granted = Vec::new();
    if let Some(p) = principal {
        anyhow::ensure!(
            principals.contains(p),
            tr("The principal no longer exists or is not visible")
        );
        let raw = query(session, &grants_sql(driver, schema, table, p))
            .await
            .context(tr(
                "Could not read explicit table grants; catalog access is required",
            ))?;
        let raw = if driver == Driver::Snowflake {
            // SHOW GRANTS hides grant options in managed access schemas.
            let schema_state = query(session, &format!("SELECT IS_MANAGED_ACCESS AS managed FROM INFORMATION_SCHEMA.SCHEMATA WHERE SCHEMA_NAME = {}", catalog_literal(driver, schema))).await?;
            let row = schema_state
                .rows
                .first()
                .context(tr("Incomplete table grant metadata"))?;
            anyhow::ensure!(
                field(&schema_state, row, "managed")?.eq_ignore_ascii_case("NO"),
                tr("Edit managed access schema grants in the SQL editor")
            );
            let current = query(session, "SELECT CURRENT_ROLE() AS name").await?;
            let role = field(
                &current,
                current
                    .rows
                    .first()
                    .context(tr("Incomplete table grant metadata"))?,
                "name",
            )?;
            let mut owns_table = false;
            for row in &raw.rows {
                if field(&raw, row, "privilege")? == "OWNERSHIP"
                    && field(&raw, row, "granted_to")? == "ROLE"
                    && field(&raw, row, "grantee_name")? == role
                {
                    owns_table = true;
                }
            }
            anyhow::ensure!(
                owns_table,
                tr("Use the table owner role to edit Snowflake table grants")
            );
            let mut normalized = ResultSet {
                columns: vec!["privilege".into(), "editable".into()],
                ..Default::default()
            };
            for row in &raw.rows {
                if field(&raw, row, "granted_to")? != "ROLE"
                    || field(&raw, row, "grantee_name")? != p.name
                {
                    continue;
                }
                let editable = field(&raw, row, "grant_option")?.eq_ignore_ascii_case("false")
                    && field(&raw, row, "granted_by")? == role
                    && field(&raw, row, "granted_by_role_type")? == "ROLE";
                normalized.rows.push(vec![
                    Some(field(&raw, row, "privilege")?.into()),
                    Some(if editable { "1" } else { "0" }.into()),
                ]);
            }
            normalized
        } else {
            raw
        };
        granted = checked_grants(driver, &raw)?;
    }
    Ok(Grants {
        principals,
        privileges: privileges(driver).iter().map(|p| (*p).into()).collect(),
        granted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn principal(name: &str, host: &str) -> Principal {
        Principal {
            name: name.into(),
            host: host.into(),
        }
    }

    fn rights(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).into()).collect()
    }

    #[test]
    fn diffs_only_named_table_rights_without_changing_grant_option() {
        let who = principal("reporter", "");
        assert_eq!(
            statements(
                Driver::Postgres,
                "public",
                "orders",
                &who,
                &rights(&["SELECT", "INSERT"]),
                &rights(&["SELECT", "UPDATE"])
            )
            .unwrap(),
            [
                "REVOKE INSERT ON TABLE \"public\".\"orders\" FROM \"reporter\" RESTRICT",
                "GRANT UPDATE ON TABLE \"public\".\"orders\" TO \"reporter\"",
            ]
        );
        assert!(
            statements(
                Driver::Postgres,
                "public",
                "orders",
                &who,
                &rights(&["SELECT"]),
                &rights(&["select", "SELECT"])
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn quotes_dialect_specific_targets_and_accounts() {
        for (driver, schema, table, who, expected) in [
            (
                Driver::Postgres,
                "s\"x",
                "t\"x",
                principal("r\";DROP ROLE x;--", ""),
                "GRANT SELECT ON TABLE \"s\"\"x\".\"t\"\"x\" TO \"r\"\";DROP ROLE x;--\"",
            ),
            (
                Driver::MySql,
                "s`x",
                "t`x",
                principal("r`'\\x", "h`%"),
                "GRANT SELECT ON `s``x`.`t``x` TO `r``'\\x`@`h``%`",
            ),
            (
                Driver::MariaDb,
                "s",
                "t",
                principal("r", "localhost"),
                "GRANT SELECT ON `s`.`t` TO `r`@`localhost`",
            ),
            (
                Driver::MsSql,
                "s]x",
                "t]x",
                principal("r]x", ""),
                "GRANT SELECT ON OBJECT::[s]]x].[t]]x] TO [r]]x]",
            ),
            (
                Driver::Oracle,
                "SCHEMA",
                "T",
                principal("REPORTER", ""),
                "GRANT SELECT ON \"SCHEMA\".\"T\" TO \"REPORTER\"",
            ),
            (
                Driver::Snowflake,
                "S",
                "T",
                principal("ROLE", ""),
                "GRANT SELECT ON TABLE \"S\".\"T\" TO ROLE \"ROLE\"",
            ),
            (
                Driver::ClickHouse,
                "s",
                "t`\\x",
                principal("r`\\x", ""),
                "GRANT SELECT ON `s`.`t\\`\\\\x` TO `r\\`\\\\x`",
            ),
        ] {
            assert_eq!(
                statements(driver, schema, table, &who, &[], &rights(&["SELECT"])).unwrap(),
                [expected]
            );
        }
    }

    #[test]
    fn rejects_privilege_injection_and_unsupported_engines() {
        for driver in Driver::ALL {
            let who = principal(
                "reporter",
                if matches!(driver, Driver::MySql | Driver::MariaDb) {
                    "%"
                } else {
                    ""
                },
            );
            assert!(
                statements(
                    driver,
                    "s",
                    "t",
                    &who,
                    &[],
                    &rights(&["SELECT; DROP TABLE t"])
                )
                .is_err()
            );
            assert!(statements(driver, "s", "t", &who, &rights(&["GRANT OPTION"]), &[]).is_err());
            if matches!(
                driver,
                Driver::Sqlite | Driver::Redis | Driver::MongoDb | Driver::InfluxDb | Driver::Kusto
            ) {
                assert!(!supported(driver));
                assert!(statements(driver, "s", "t", &who, &[], &[]).is_err());
            } else {
                assert!(supported(driver));
            }
        }
        assert!(
            statements(
                Driver::Postgres,
                "s",
                "t",
                &principal("r", "host"),
                &[],
                &[]
            )
            .is_err()
        );
        assert!(statements(Driver::Postgres, "s", "t\0", &principal("r", ""), &[], &[]).is_err());
    }

    #[test]
    fn refuses_unsafe_or_incomplete_catalog_snapshots() {
        let row = |privilege: &str, editable: &str| super::super::ResultSet {
            columns: vec!["privilege".into(), "editable".into()],
            rows: vec![vec![Some(privilege.into()), Some(editable.into())]],
            ..Default::default()
        };
        assert_eq!(
            checked_grants(Driver::MySql, &row("Select,Insert", "1")).unwrap(),
            ["INSERT", "SELECT"]
        );
        assert!(checked_grants(Driver::Postgres, &row("SELECT", "0")).is_err());
        assert!(checked_grants(Driver::MySql, &row("Select,Grant", "1")).is_err());
        assert!(checked_grants(Driver::Postgres, &row("SELECT", "unknown")).is_err());
        let mut truncated = row("SELECT", "1");
        truncated.truncated = true;
        assert!(checked_grants(Driver::Postgres, &truncated).is_err());
        assert!(checked_grants(Driver::Postgres, &super::super::ResultSet::default()).is_err());
    }
}
