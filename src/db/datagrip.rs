//! Import from DataGrip: the data sources of a project's `.idea/dataSources.xml` (users come from
//! `dataSources.local.xml` next to it). The files are only read. Passwords stay in DataGrip's
//! own store and aren't imported.

use super::url::{apply_connection_url, apply_jdbc_url, take_tls_params};
use super::{ConnectionConfig, Driver};
use crate::i18n::{tr, trf};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A data source of the file.
#[derive(Debug, Clone)]
pub struct Found {
    pub name: String,
    /// DataGrip's folder, empty at the top level.
    pub folder: String,
    /// The connection to add (id 0), or why it can't be.
    pub config: Result<ConnectionConfig, String>,
}

/// The `dataSources.xml` of the projects in `~/DataGripProjects` and of DataGrip's global data
/// sources.
pub fn default_files() -> Vec<PathBuf> {
    let subdirs = |dir: PathBuf| -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        dirs
    };
    let mut files = Vec::new();
    if let Some(home) = dirs::home_dir() {
        files.extend(
            subdirs(home.join("DataGripProjects"))
                .into_iter()
                .map(|p| p.join(".idea").join("dataSources.xml")),
        );
    }
    if let Some(config) = dirs::config_dir() {
        let versions = subdirs(config.join("JetBrains")).into_iter().filter(|d| {
            d.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("DataGrip"))
        });
        // Newest version first.
        let mut global: Vec<PathBuf> = versions
            .map(|d| d.join("options").join("dataSources.xml"))
            .collect();
        global.reverse();
        files.extend(global);
    }
    files.retain(|f| f.is_file());
    files
}

pub fn read(path: &Path) -> anyhow::Result<Vec<Found>> {
    let xml = std::fs::read_to_string(path)?;
    let local =
        std::fs::read_to_string(path.with_file_name("dataSources.local.xml")).unwrap_or_default();
    let project = path
        .parent()
        .filter(|d| d.file_name().is_some_and(|n| n == ".idea"))
        .and_then(Path::parent);
    Ok(parse(&xml, &local, project))
}

pub fn parse(xml: &str, local: &str, project: Option<&Path>) -> Vec<Found> {
    let users: HashMap<String, String> = elements(local, "data-source")
        .into_iter()
        .filter_map(|(attrs, inner)| Some((attr(attrs, "uuid")?, child(inner, "user-name")?)))
        .collect();
    elements(xml, "data-source")
        .into_iter()
        .map(|(attrs, inner)| {
            let name = attr(attrs, "name").unwrap_or_default();
            let folder = attr(attrs, "group")
                .or_else(|| child(inner, "group"))
                .unwrap_or_default();
            let user = attr(attrs, "uuid")
                .and_then(|u| users.get(&u).cloned())
                .or_else(|| child(inner, "user-name"))
                .unwrap_or_default();
            let driver_ref = child(inner, "driver-ref").unwrap_or_default();
            let url = child(inner, "jdbc-url").unwrap_or_default();
            let config = config(&name, &driver_ref, &url, &user, project);
            Found {
                name,
                folder,
                config,
            }
        })
        .collect()
}

/// The connection DataGrip's driver and JDBC URL describe.
fn config(
    name: &str,
    driver_ref: &str,
    url: &str,
    user: &str,
    project: Option<&Path>,
) -> Result<ConnectionConfig, String> {
    let mut url = url.trim().to_string();
    for (var, dir) in [
        ("$USER_HOME$", dirs::home_dir()),
        ("$PROJECT_DIR$", project.map(Path::to_path_buf)),
        ("$ProjectFileDir$", project.map(Path::to_path_buf)),
    ] {
        if let Some(dir) = dir {
            url = url.replace(var, &dir.to_string_lossy());
        }
    }
    let lower = url.to_ascii_lowercase();
    let after = |prefix: &str| url[prefix.len()..].to_string();
    // The driver, and the URL in the form `apply_jdbc_url` (JDBC) or `apply_connection_url` takes.
    let (driver, jdbc, url) = if lower.starts_with("jdbc:postgresql:") {
        (Driver::Postgres, true, url)
    } else if lower.starts_with("jdbc:mysql:") && driver_ref.starts_with("mariadb") {
        (
            Driver::MariaDb,
            true,
            format!("jdbc:mariadb:{}", after("jdbc:mysql:")),
        )
    } else if lower.starts_with("jdbc:mysql:") {
        (Driver::MySql, true, url)
    } else if lower.starts_with("jdbc:mariadb:") {
        (Driver::MariaDb, true, url)
    } else if lower.starts_with("jdbc:sqlserver:") {
        (Driver::MsSql, true, url)
    } else if lower.starts_with("jdbc:jtds:sqlserver://") {
        // host:port/database;params
        let rest = after("jdbc:jtds:sqlserver://");
        let (main, params) = rest.split_at(rest.find(';').unwrap_or(rest.len()));
        let main = match main.split_once('/') {
            Some((authority, db)) if !db.is_empty() => format!("{authority};database={db}"),
            _ => main.trim_end_matches('/').to_string(),
        };
        (
            Driver::MsSql,
            true,
            format!("jdbc:sqlserver://{main}{params}"),
        )
    } else if lower.starts_with("jdbc:oracle:thin:@//") {
        (Driver::Oracle, true, url)
    } else if lower.starts_with("jdbc:oracle:thin:@") && !lower.contains("(description") {
        // host:port/service or host:port:SID
        let rest = after("jdbc:oracle:thin:@");
        let rest = match rest.matches(':').count() {
            2 => rest
                .rsplitn(2, ':')
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("/"),
            _ => rest,
        };
        (Driver::Oracle, true, format!("jdbc:oracle:thin:@//{rest}"))
    } else if lower.starts_with("jdbc:sqlite:") {
        (Driver::Sqlite, true, url)
    } else if lower.starts_with("jdbc:snowflake:") {
        (Driver::Snowflake, true, url)
    } else if lower.starts_with("jdbc:clickhouse://") || lower.starts_with("jdbc:ch://") {
        (
            Driver::ClickHouse,
            false,
            format!("clickhouse://{}", &url[url.find("//").unwrap_or(0) + 2..]),
        )
    } else if lower.starts_with("jdbc:redis://") {
        (Driver::Redis, false, after("jdbc:"))
    } else if lower.starts_with("mongodb://") || lower.starts_with("mongodb+srv://") {
        (Driver::MongoDb, false, url)
    } else if lower.starts_with("jdbc:mongodb") {
        (Driver::MongoDb, false, after("jdbc:"))
    } else {
        let what = if driver_ref.is_empty() {
            url.split("//")
                .next()
                .unwrap_or(&url)
                .trim_end_matches(':')
                .to_string()
        } else {
            driver_ref.to_string()
        };
        return Err(trf("{driver} isn't supported", &[("driver", &what)]));
    };
    let mut cfg = ConnectionConfig::new(0, driver);
    cfg.name = name.to_string();
    let ok = if jdbc {
        apply_jdbc_url(&mut cfg, &url)
    } else {
        apply_connection_url(&mut cfg, &url)
    };
    if !ok {
        return Err(tr("Unreadable URL").into());
    }
    match driver {
        // DataGrip's ClickHouse driver speaks HTTP; DataCraft the native protocol.
        Driver::ClickHouse if cfg.port == 8123 => cfg.port = driver.default_port(),
        Driver::ClickHouse if cfg.port == 8443 => {
            cfg.port = super::clickhouse::TLS_PORT;
            cfg.set_property("tls", "true");
        }
        // Azure Data Explorer through DataGrip's SQL Server driver.
        Driver::MsSql if cfg.host.to_ascii_lowercase().contains(".kusto.") => {
            let (host, database) = (cfg.host.clone(), cfg.database.clone());
            cfg = ConnectionConfig::new(0, Driver::Kusto);
            (cfg.name, cfg.host, cfg.database) = (name.to_string(), host, database);
            return Ok(cfg);
        }
        _ => {}
    }
    take_tls_params(&mut cfg);
    if driver != Driver::Sqlite {
        cfg.user = user.to_string();
        cfg.ask_password = true;
    }
    Ok(cfg)
}

/// The `<tag …>…</tag>` elements, as (attributes, content), in document order.
fn elements<'a>(xml: &'a str, tag: &str) -> Vec<(&'a str, &'a str)> {
    let (open, close) = (format!("<{tag}"), format!("</{tag}>"));
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        // `<data-source` but not `<data-sources`.
        if !after.starts_with([' ', '>', '/', '\n', '\r', '\t']) {
            rest = after;
            continue;
        }
        let Some(end) = after.find('>') else { break };
        let attrs = &after[..end];
        if let Some(attrs) = attrs.strip_suffix('/') {
            out.push((attrs, ""));
            rest = &after[end + 1..];
            continue;
        }
        let body = &after[end + 1..];
        let Some(j) = body.find(&close) else { break };
        out.push((attrs, &body[..j]));
        rest = &body[j + close.len()..];
    }
    out
}

/// An attribute's value (`name="…"`), unescaped.
fn attr(attrs: &str, name: &str) -> Option<String> {
    let key = format!("{name}=\"");
    let mut from = 0;
    while let Some(i) = attrs[from..].find(&key).map(|i| i + from) {
        if i == 0 || attrs.as_bytes()[i - 1].is_ascii_whitespace() {
            let value = &attrs[i + key.len()..];
            return value.find('"').map(|end| unescape(&value[..end]));
        }
        from = i + key.len();
    }
    None
}

/// The text of the first `<tag>` inside `inner`.
fn child(inner: &str, tag: &str) -> Option<String> {
    elements(inner, tag)
        .first()
        .map(|(_, text)| unescape(text.trim()))
        .filter(|t| !t.is_empty())
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';').filter(|&e| e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let ch = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tls::TlsMode;

    const XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<project version="4">
  <component name="DataSourceManagerImpl" format="xml" multifile-model="true">
    <data-source source="LOCAL" name="orders@prod" group="Production" uuid="u1">
      <driver-ref>postgresql</driver-ref>
      <jdbc-driver>org.postgresql.Driver</jdbc-driver>
      <jdbc-url>jdbc:postgresql://db.example.com:5433/orders?sslmode=verify-full&amp;connectTimeout=5</jdbc-url>
    </data-source>
    <data-source source="LOCAL" name="shop &amp; stock" uuid="u2">
      <driver-ref>mariadb</driver-ref>
      <jdbc-url>jdbc:mysql://maria.example.com/shop</jdbc-url>
    </data-source>
    <data-source source="LOCAL" name="erp" uuid="u3">
      <driver-ref>sqlserver.jb</driver-ref>
      <jdbc-url>jdbc:jtds:sqlserver://sql.example.com:1434/erp;instance=main</jdbc-url>
    </data-source>
    <data-source source="LOCAL" name="local file" uuid="u4">
      <driver-ref>sqlite.xerial</driver-ref>
      <jdbc-url>jdbc:sqlite:$PROJECT_DIR$/data/app.db</jdbc-url>
    </data-source>
    <data-source source="LOCAL" name="legacy" uuid="u5">
      <driver-ref>oracle</driver-ref>
      <jdbc-url>jdbc:oracle:thin:@ora.example.com:1521:ORCL</jdbc-url>
    </data-source>
    <data-source source="LOCAL" name="events" uuid="u6">
      <driver-ref>clickhouse</driver-ref>
      <jdbc-url>jdbc:clickhouse://ch.example.com:8123/logs</jdbc-url>
    </data-source>
    <data-source source="LOCAL" name="cache" uuid="u7">
      <driver-ref>redis</driver-ref>
      <jdbc-url>jdbc:redis://cache.example.com:6380/2</jdbc-url>
    </data-source>
    <data-source source="LOCAL" name="docs" uuid="u8">
      <driver-ref>mongo</driver-ref>
      <jdbc-url>mongodb+srv://cluster0.example.net/app?retryWrites=true</jdbc-url>
    </data-source>
    <data-source source="LOCAL" name="warehouse" uuid="u9">
      <driver-ref>redshift</driver-ref>
      <jdbc-url>jdbc:redshift://rs.example.com:5439/dev</jdbc-url>
    </data-source>
  </component>
</project>"#;

    const LOCAL: &str = r#"<project version="4">
  <component name="dataSourceStorageLocal">
    <data-source name="orders@prod" uuid="u1">
      <secret-storage>master_key</secret-storage>
      <user-name>app_reader</user-name>
    </data-source>
    <data-source name="erp" uuid="u3"><user-name>sa</user-name></data-source>
  </component>
</project>"#;

    #[test]
    fn reads_datagrip_data_sources() {
        let found = parse(XML, LOCAL, Some(Path::new("/projects/shop")));
        assert_eq!(found.len(), 9);
        let ok = |i: usize| found[i].config.clone().unwrap();

        let pg = ok(0);
        assert_eq!(
            (found[0].name.as_str(), found[0].folder.as_str()),
            ("orders@prod", "Production")
        );
        assert_eq!(
            (
                pg.driver,
                pg.host.as_str(),
                pg.port,
                pg.database.as_str(),
                pg.user.as_str()
            ),
            (
                Driver::Postgres,
                "db.example.com",
                5433,
                "orders",
                "app_reader"
            )
        );
        assert_eq!(
            (pg.tls.mode, pg.url_extra.as_str()),
            (TlsMode::VerifyFull, "?connectTimeout=5")
        );
        assert!(pg.ask_password && pg.password.is_empty());

        let maria = ok(1);
        assert_eq!(
            (
                found[1].name.as_str(),
                maria.driver,
                maria.host.as_str(),
                maria.port,
                maria.user.as_str()
            ),
            (
                "shop & stock",
                Driver::MariaDb,
                "maria.example.com",
                3306,
                ""
            )
        );

        let ms = ok(2);
        assert_eq!(
            (
                ms.driver,
                ms.host.as_str(),
                ms.port,
                ms.database.as_str(),
                ms.user.as_str()
            ),
            (Driver::MsSql, "sql.example.com", 1434, "erp", "sa")
        );

        let lite = ok(3);
        assert_eq!(
            (lite.driver, lite.file_path.as_str(), lite.ask_password),
            (Driver::Sqlite, "/projects/shop/data/app.db", false)
        );

        let ora = ok(4);
        assert_eq!(
            (
                ora.driver,
                ora.host.as_str(),
                ora.port,
                ora.database.as_str()
            ),
            (Driver::Oracle, "ora.example.com", 1521, "ORCL")
        );

        // DataGrip's ClickHouse port is the HTTP one.
        let ch = ok(5);
        assert_eq!(
            (ch.driver, ch.port, ch.database.as_str()),
            (Driver::ClickHouse, 9000, "logs")
        );

        let redis = ok(6);
        assert_eq!(
            (
                redis.driver,
                redis.host.as_str(),
                redis.port,
                redis.database.as_str()
            ),
            (Driver::Redis, "cache.example.com", 6380, "2")
        );

        let mongo = ok(7);
        assert_eq!(
            (
                mongo.driver,
                mongo.host.as_str(),
                mongo.database.as_str(),
                mongo.property("srv")
            ),
            (Driver::MongoDb, "cluster0.example.net", "app", Some("true"))
        );

        assert!(found[8].config.as_ref().unwrap_err().contains("redshift"));
    }

    #[test]
    fn unescapes_xml() {
        assert_eq!(
            unescape("a &amp; b &lt;c&gt; &quot;d&quot; &#233; &#xE9; & x"),
            "a & b <c> \"d\" é é & x"
        );
        assert_eq!(
            attr(r#"source="LOCAL" name="x" uuid="1""#, "name").as_deref(),
            Some("x")
        );
        assert_eq!(
            attr(r#"dbname="no" name="yes""#, "name").as_deref(),
            Some("yes")
        );
    }
}
