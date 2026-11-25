//! "New Database" options per engine: character set and collation (MySQL / MariaDB), owner,
//! encoding, template, locale and connection limit (PostgreSQL), collation (SQL Server), and
//! transient / retention / comment (Snowflake). The choices come from the server.

use super::{Driver, Session};
use crate::edit::literal;
use crate::ui::quote_ident_if_needed;

/// What the user picks; empty fields are left to the server's defaults.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DatabaseOptions {
    pub charset: String,
    pub collation: String,
    pub comment: String,
    pub owner: String,
    pub encoding: String,
    pub template: String,
    pub locale: String,
    pub connection_limit: String,
    pub transient: bool,
    pub retention_days: String,
}

/// The server's lists for the dialog's menus, and its defaults.
#[derive(Clone, Debug, Default)]
pub struct DatabaseChoices {
    /// (character set, its default collation).
    pub charsets: Vec<(String, String)>,
    /// (collation, character set); SQL Server: (collation, "").
    pub collations: Vec<(String, String)>,
    pub default_charset: String,
    pub default_collation: String,
    /// PostgreSQL roles that can own the database, and the connected one.
    pub roles: Vec<String>,
    pub current_role: String,
    /// MariaDB (10.5+) takes a COMMENT; MySQL does not.
    pub mariadb: bool,
}

/// PostgreSQL server encodings (the client-only ones are left out).
pub const PG_ENCODINGS: &[&str] = &[
    "UTF8",
    "SQL_ASCII",
    "LATIN1",
    "LATIN2",
    "LATIN9",
    "WIN1250",
    "WIN1251",
    "WIN1252",
    "EUC_JP",
    "EUC_KR",
    "EUC_CN",
    "ISO_8859_5",
    "KOI8R",
    "KOI8U",
];

fn col(rs: &[super::ResultSet], row: usize, c: usize) -> String {
    rs.first()
        .and_then(|r| r.rows.get(row))
        .and_then(|r| r.get(c).cloned().flatten())
        .unwrap_or_default()
}

fn pairs(rs: Vec<super::ResultSet>) -> Vec<(String, String)> {
    rs.into_iter()
        .next()
        .map(|r| {
            r.rows
                .into_iter()
                .map(|row| {
                    (
                        row.first().cloned().flatten().unwrap_or_default(),
                        row.get(1).cloned().flatten().unwrap_or_default(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

impl Session {
    /// Lists for "New Database" (an engine without options gives empty lists).
    pub async fn database_choices(&mut self, driver: Driver) -> anyhow::Result<DatabaseChoices> {
        let mut c = DatabaseChoices::default();
        match driver {
            Driver::MySql | Driver::MariaDb => {
                c.charsets = pairs(self.execute("SELECT CHARACTER_SET_NAME, DEFAULT_COLLATE_NAME FROM information_schema.CHARACTER_SETS ORDER BY 1", 10_000).await?);
                c.collations = pairs(self.execute("SELECT COLLATION_NAME, CHARACTER_SET_NAME FROM information_schema.COLLATIONS ORDER BY 1", 10_000).await?);
                let d = self
                    .execute(
                        "SELECT @@character_set_server, @@collation_server, VERSION()",
                        1,
                    )
                    .await?;
                c.default_charset = col(&d, 0, 0);
                c.default_collation = col(&d, 0, 1);
                c.mariadb = col(&d, 0, 2).to_ascii_lowercase().contains("mariadb");
            }
            Driver::Postgres => {
                c.roles = pairs(
                    self.execute(
                        "SELECT rolname, '' FROM pg_roles WHERE rolname !~ '^pg_' ORDER BY 1",
                        10_000,
                    )
                    .await?,
                )
                .into_iter()
                .map(|(r, _)| r)
                .collect();
                c.current_role = col(&self.execute("SELECT current_user", 1).await?, 0, 0);
            }
            Driver::MsSql => {
                c.collations = pairs(
                    self.execute(
                        "SELECT name, '' FROM sys.fn_helpcollations() ORDER BY name",
                        20_000,
                    )
                    .await?,
                );
                c.default_collation = col(
                    &self
                        .execute(
                            "SELECT CAST(SERVERPROPERTY('Collation') AS NVARCHAR(128))",
                            1,
                        )
                        .await?,
                    0,
                    0,
                );
            }
            _ => {}
        }
        Ok(c)
    }
}

/// `CREATE DATABASE` with the chosen options. `mariadb`: whether COMMENT is allowed.
pub fn create_database_sql(
    driver: Driver,
    name: &str,
    o: &DatabaseOptions,
    mariadb: bool,
) -> String {
    let q = |s: &str| quote_ident_if_needed(driver, s.trim());
    let lit = |s: &str| literal(driver, &Some(s.trim().to_string()));
    let mut sql = match driver {
        Driver::Snowflake if o.transient => format!("create transient database {}", q(name)),
        _ => format!("create database {}", q(name)),
    };
    let set = |s: &str| !s.trim().is_empty();
    match driver {
        Driver::MySql | Driver::MariaDb => {
            if set(&o.charset) {
                sql.push_str(&format!("\n    character set {}", o.charset.trim()));
            }
            if set(&o.collation) {
                sql.push_str(&format!("\n    collate {}", o.collation.trim()));
            }
            if mariadb && set(&o.comment) {
                sql.push_str(&format!("\n    comment {}", lit(&o.comment)));
            }
        }
        Driver::Postgres => {
            let mut opts = Vec::new();
            if set(&o.owner) {
                opts.push(format!("owner = {}", q(&o.owner)));
            }
            // A new encoding or locale needs template0: template1 may hold data in another one.
            let template = if set(&o.template) {
                o.template.trim().to_string()
            } else if set(&o.encoding) || set(&o.locale) {
                "template0".into()
            } else {
                String::new()
            };
            if !template.is_empty() {
                opts.push(format!("template = {}", q(&template)));
            }
            if set(&o.encoding) {
                opts.push(format!("encoding = {}", lit(&o.encoding)));
            }
            if set(&o.locale) {
                opts.push(format!("locale = {}", lit(&o.locale)));
            }
            if let Ok(n) = o.connection_limit.trim().parse::<i64>() {
                opts.push(format!("connection limit = {n}"));
            }
            if !opts.is_empty() {
                sql.push_str(&format!("\n    {}", opts.join("\n    ")));
            }
        }
        Driver::MsSql if set(&o.collation) => {
            sql.push_str(&format!("\n    collate {}", o.collation.trim()))
        }
        Driver::Snowflake => {
            if let Ok(n) = o.retention_days.trim().parse::<u32>() {
                sql.push_str(&format!("\n    data_retention_time_in_days = {n}"));
            }
            if set(&o.comment) {
                sql.push_str(&format!("\n    comment = {}", lit(&o.comment)));
            }
        }
        _ => {}
    }
    sql
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_per_engine() {
        let none = DatabaseOptions::default();
        assert_eq!(
            create_database_sql(Driver::Postgres, "shop", &none, false),
            "create database shop"
        );
        let my = DatabaseOptions {
            charset: "utf8mb4".into(),
            collation: "utf8mb4_general_ci".into(),
            comment: "it's".into(),
            ..none.clone()
        };
        assert_eq!(
            create_database_sql(Driver::MySql, "Shop", &my, false),
            "create database Shop\n    character set utf8mb4\n    collate utf8mb4_general_ci"
        );
        assert_eq!(
            create_database_sql(Driver::MariaDb, "shop", &my, true),
            "create database shop\n    character set utf8mb4\n    collate utf8mb4_general_ci\n    comment 'it''s'"
        );
        let pg = DatabaseOptions {
            owner: "App".into(),
            encoding: "UTF8".into(),
            locale: "en_US.UTF-8".into(),
            connection_limit: "20".into(),
            ..none.clone()
        };
        assert_eq!(
            create_database_sql(Driver::Postgres, "shop", &pg, false),
            "create database shop\n    owner = \"App\"\n    template = template0\n    encoding = 'UTF8'\n    locale = 'en_US.UTF-8'\n    connection limit = 20"
        );
        let ms = DatabaseOptions {
            collation: "Latin1_General_CI_AS".into(),
            ..none.clone()
        };
        assert_eq!(
            create_database_sql(Driver::MsSql, "My Shop", &ms, false),
            "create database [My Shop]\n    collate Latin1_General_CI_AS"
        );
        let sf = DatabaseOptions {
            transient: true,
            retention_days: "0".into(),
            comment: "scratch".into(),
            ..none
        };
        assert_eq!(
            create_database_sql(Driver::Snowflake, "SCRATCH", &sf, false),
            "create transient database SCRATCH\n    data_retention_time_in_days = 0\n    comment = 'scratch'"
        );
    }
}
