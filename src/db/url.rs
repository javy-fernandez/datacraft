//! The connection dialog's URL field: DataGrip's JDBC URL for each driver, built from the
//! General tab fields and parsed back into them when the URL is edited. It also takes the URLs
//! other tools use (`postgresql://user:pw@host/db`, `mongodb+srv://…`), and "Copy as URL" writes
//! them.

use super::mongo::enc;
use super::tls::{TlsMode, TlsSettings};
use super::{AuthMode, ConnectionConfig, Driver, snowflake};

/// The URL DataGrip shows for these settings (default URL templates of its drivers), followed by
/// the parameters that have no field (`url_extra`).
pub fn jdbc_url(cfg: &ConnectionConfig) -> String {
    let host = cfg.host.trim();
    let db = cfg.database.trim();
    let extra = cfg.url_extra.trim();
    let authority = if cfg.port == 0 {
        host.to_string()
    } else {
        format!("{host}:{}", cfg.port)
    };
    let slash_db = if db.is_empty() {
        String::new()
    } else {
        format!("/{db}")
    };
    match cfg.driver {
        Driver::Postgres => format!("jdbc:postgresql://{authority}{slash_db}{extra}"),
        Driver::MySql => format!("jdbc:mysql://{authority}{slash_db}{extra}"),
        Driver::MariaDb => format!("jdbc:mariadb://{authority}{slash_db}{extra}"),
        Driver::MsSql if db.is_empty() => format!("jdbc:sqlserver://{authority}{extra}"),
        Driver::MsSql => format!("jdbc:sqlserver://{authority};database={db}{extra}"),
        Driver::Oracle => format!("jdbc:oracle:thin:@//{authority}{slash_db}{extra}"),
        Driver::Sqlite => format!("jdbc:sqlite:{}", cfg.file_path.trim()),
        // DuckDB's JDBC URL: `jdbc:duckdb:` alone is an in-memory database.
        Driver::DuckDb => format!("jdbc:duckdb:{}", cfg.file_path.trim()),
        Driver::Snowflake => snowflake::jdbc_url(cfg),
        Driver::Redis if truthy(cfg.property("tls")) => {
            format!("rediss://{authority}{slash_db}{extra}")
        }
        Driver::Redis => format!("redis://{authority}{slash_db}{extra}"),
        Driver::MongoDb if mongo_srv(cfg) => format!("mongodb+srv://{host}{slash_db}{extra}"),
        Driver::MongoDb => format!("mongodb://{authority}{slash_db}{extra}"),
        Driver::ClickHouse => format!("clickhouse://{authority}{slash_db}{extra}"),
        Driver::InfluxDb => format!("influxdb://{authority}{slash_db}{extra}"),
        // The cluster URI, as Kusto's own tools take it.
        Driver::Kusto => format!("{}{slash_db}{extra}", super::kusto::cluster_url(cfg)),
        // The API's base, as the server's own UI show it.
        Driver::Prometheus => format!("{}{extra}", super::prometheus::base_url(cfg)),
    }
}

/// Sets the fields the URL carries; parameters without a field go to `url_extra` so they survive
/// the round trip. Returns false, changing nothing, when the URL is not one of this driver's or
/// its port is not a valid port (the field then keeps the text until it is fixed).
pub fn apply_jdbc_url(cfg: &mut ConnectionConfig, url: &str) -> bool {
    let url = url.trim();
    let prefix = match cfg.driver {
        Driver::Postgres => "jdbc:postgresql://",
        Driver::MySql => "jdbc:mysql://",
        Driver::MariaDb => "jdbc:mariadb://",
        Driver::MsSql => "jdbc:sqlserver://",
        Driver::Oracle => "jdbc:oracle:thin:@//",
        Driver::Sqlite => {
            let Some(path) = url.strip_prefix("jdbc:sqlite:") else {
                return false;
            };
            cfg.file_path = path.to_string();
            return true;
        }
        Driver::DuckDb => {
            let Some(path) = url.strip_prefix("jdbc:duckdb:") else {
                return false;
            };
            cfg.file_path = path.to_string();
            return true;
        }
        Driver::Snowflake => return snowflake::apply_jdbc_url(cfg, url),
        Driver::Redis => "redis://",
        Driver::MongoDb => "mongodb://",
        Driver::ClickHouse => "clickhouse://",
        Driver::InfluxDb => {
            // `http(s)://host:8086` as InfluxDB's own tools take it: https switches TLS on.
            for (scheme, tls) in [("http://", false), ("https://", true)] {
                if url
                    .get(..scheme.len())
                    .is_some_and(|p| p.eq_ignore_ascii_case(scheme))
                {
                    let mut c = cfg.clone();
                    c.set_property("tls", if tls { "true" } else { "" });
                    let ok = apply_jdbc_url(
                        &mut c,
                        &format!("influxdb://{}", url[scheme.len()..].trim_end_matches('/')),
                    );
                    if ok {
                        *cfg = c;
                    }
                    return ok;
                }
            }
            "influxdb://"
        }
        Driver::Kusto => {
            // `https://help.kusto.windows.net/Samples`; `http://` (the emulator) turns TLS off.
            for (scheme, tls, default_port) in [("https://", true, 443), ("http://", false, 80)] {
                if url
                    .get(..scheme.len())
                    .is_some_and(|p| p.eq_ignore_ascii_case(scheme))
                {
                    let rest = &url[scheme.len()..];
                    let (main, extra) = rest.split_at(rest.find('?').unwrap_or(rest.len()));
                    let (authority, db) = main.split_once('/').unwrap_or((main, ""));
                    let Some((host, port)) = parse_authority(authority, default_port) else {
                        return false;
                    };
                    (cfg.host, cfg.port, cfg.database, cfg.url_extra) = (
                        host,
                        port,
                        db.trim_end_matches('/').to_string(),
                        extra.to_string(),
                    );
                    cfg.set_property("tls", if tls { "" } else { "false" });
                    return true;
                }
            }
            return false;
        }
        Driver::Prometheus => {
            // `http(s)://host:port/prefix?…`: https switches TLS on, and the path prefix stays in
            // the host field (`vmselect/select/0/prometheus`).
            for (scheme, tls, default_port) in [("https://", true, 443), ("http://", false, 80)] {
                if url
                    .get(..scheme.len())
                    .is_some_and(|p| p.eq_ignore_ascii_case(scheme))
                {
                    let rest = &url[scheme.len()..];
                    let (main, extra) = rest.split_at(rest.find('?').unwrap_or(rest.len()));
                    let (authority, path) = main.split_at(main.find('/').unwrap_or(main.len()));
                    let Some((host, port)) = parse_authority(authority, default_port) else {
                        return false;
                    };
                    (cfg.host, cfg.port, cfg.url_extra) = (
                        format!("{host}{}", path.trim_end_matches('/')),
                        port,
                        extra.to_string(),
                    );
                    cfg.set_property("tls", if tls { "true" } else { "" });
                    return true;
                }
            }
            return false;
        }
    };
    let Some(rest) = url.strip_prefix(prefix) else {
        return false;
    };
    let default_port = cfg.driver.default_port();
    if cfg.driver == Driver::MsSql {
        // host:port;database=db;other=…
        let mut parts = rest.split(';');
        let Some((host, port)) = parse_authority(parts.next().unwrap_or(""), default_port) else {
            return false;
        };
        let (mut database, mut extra) = (String::new(), String::new());
        for part in parts.filter(|p| !p.trim().is_empty()) {
            match part.split_once('=') {
                Some((k, v))
                    if k.trim().eq_ignore_ascii_case("database")
                        || k.trim().eq_ignore_ascii_case("databaseName") =>
                {
                    database = v.trim().to_string();
                }
                _ => {
                    extra.push(';');
                    extra.push_str(part);
                }
            }
        }
        (cfg.host, cfg.port, cfg.database, cfg.url_extra) = (host, port, database, extra);
        return true;
    }
    // host:port/db?params
    let split = rest.find(['?', ';']).unwrap_or(rest.len());
    let (main, extra) = rest.split_at(split);
    let (authority, db) = main.split_once('/').unwrap_or((main, ""));
    let Some((host, port)) = parse_authority(authority, default_port) else {
        return false;
    };
    (cfg.host, cfg.port, cfg.database, cfg.url_extra) =
        (host, port, db.to_string(), extra.to_string());
    true
}

/// Schemes of the connection URLs other tools take, and their engine.
const SCHEMES: [(&str, Driver); 13] = [
    ("postgresql://", Driver::Postgres),
    ("postgres://", Driver::Postgres),
    ("mysql://", Driver::MySql),
    ("mariadb://", Driver::MariaDb),
    ("sqlserver://", Driver::MsSql),
    ("mssql://", Driver::MsSql),
    ("mongodb+srv://", Driver::MongoDb),
    ("mongodb://", Driver::MongoDb),
    ("rediss://", Driver::Redis),
    ("redis://", Driver::Redis),
    ("clickhouse://", Driver::ClickHouse),
    ("sqlite:", Driver::Sqlite),
    ("duckdb:", Driver::DuckDb),
];

fn scheme_of(url: &str) -> Option<(&'static str, Driver)> {
    SCHEMES.into_iter().find(|(s, _)| {
        url.get(..s.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(s))
    })
}

fn truthy(v: Option<&str>) -> bool {
    v.is_some_and(|v| v.eq_ignore_ascii_case("true") || v == "1")
}

/// `mongodb+srv://`: Atlas, or the `srv` property.
fn mongo_srv(cfg: &ConnectionConfig) -> bool {
    cfg.host.trim().ends_with(".mongodb.net") || truthy(cfg.property("srv"))
}

/// A SQLite or DuckDB file's path, pasted as is (`/data/app.db`, `~/app.sqlite`,
/// `C:\data\app.db`, `~/sales.duckdb`), and its engine.
fn file_path_driver(text: &str) -> Option<Driver> {
    let lower = text.to_ascii_lowercase();
    let absolute = text.starts_with('/')
        || text.starts_with("~/")
        || text.get(1..3).is_some_and(|s| s == ":\\" || s == ":/");
    if !absolute || text.contains("://") {
        return None;
    }
    if [".duckdb", ".ddb"].iter().any(|e| lower.ends_with(e)) {
        Some(Driver::DuckDb)
    } else if [".db", ".sqlite", ".sqlite3", ".db3"]
        .iter()
        .any(|e| lower.ends_with(e))
    {
        Some(Driver::Sqlite)
    } else {
        None
    }
}

/// The URL field's text: this driver's JDBC URL, or another tool's connection URL of any engine
/// (or a SQLite file's path), which switches the driver and also fills user and password.
/// Returns false, changing nothing, when it is neither.
pub fn apply_url(cfg: &mut ConnectionConfig, url: &str) -> bool {
    let url = url.trim();
    let driver = match scheme_of(url) {
        Some((_, d)) => Some(d),
        None => file_path_driver(url),
    };
    if let Some(driver) = driver {
        let mut c = cfg.clone();
        c.change_driver(driver);
        if apply_connection_url(&mut c, url) {
            *cfg = c;
            return true;
        }
    }
    apply_jdbc_url(cfg, url)
}

/// `scheme://[user[:password]@]host[:port][/database][?params]` for `cfg.driver`, which the
/// scheme must be one of; SQL Server also takes `;database=…;user=…` parameters. The password
/// is only replaced when the URL has one.
pub(crate) fn apply_connection_url(cfg: &mut ConnectionConfig, url: &str) -> bool {
    if matches!(cfg.driver, Driver::Sqlite | Driver::DuckDb) {
        let schemes = if cfg.driver == Driver::Sqlite {
            ["sqlite://", "sqlite:"]
        } else {
            ["duckdb://", "duckdb:"]
        };
        let path = schemes.iter().find_map(|s| {
            url.get(..s.len())
                .filter(|p| p.eq_ignore_ascii_case(s))
                .map(|_| &url[s.len()..])
        });
        let path = path.unwrap_or(url);
        // `sqlite:////abs` (SQLAlchemy) is `/abs`.
        let path = if path.starts_with("//") {
            &path[path.len() - path.trim_start_matches('/').len() - 1..]
        } else {
            path
        };
        let path = match (path.strip_prefix("~/"), dirs::home_dir()) {
            (Some(rest), Some(home)) => home.join(rest).to_string_lossy().into_owned(),
            _ => path.to_string(),
        };
        if path.trim().is_empty() {
            return false;
        }
        cfg.file_path = path;
        return true;
    }
    let Some((scheme, driver)) = scheme_of(url) else {
        return false;
    };
    if driver != cfg.driver {
        return false;
    }
    let rest = &url[scheme.len()..];
    let (main, query) = rest.split_at(rest.find(['?', ';']).unwrap_or(rest.len()));
    let (userinfo, hostpart) = match main.rfind('@') {
        Some(i) => (Some(&main[..i]), &main[i + 1..]),
        None => (None, main),
    };
    let (authority, path) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    if authority.is_empty() {
        return false;
    }
    let Some((host, port)) = parse_authority(authority, cfg.driver.default_port()) else {
        return false;
    };
    let (mut user, mut password) = match userinfo.map(|u| u.split_once(':').unwrap_or((u, ""))) {
        Some((u, p)) => (
            Some(decode(u)),
            userinfo.filter(|u| u.contains(':')).map(|_| decode(p)),
        ),
        None => (None, None),
    };
    let mut database = decode(path.trim_end_matches('/'));
    let mut extra = Vec::new();
    for param in query
        .trim_start_matches(['?', ';'])
        .split(['&', ';'])
        .filter(|p| !p.trim().is_empty())
    {
        let (k, v) = param.split_once('=').unwrap_or((param, ""));
        let (key, value) = (k.trim().to_ascii_lowercase(), decode(v.trim()));
        match (cfg.driver, key.as_str()) {
            (Driver::MsSql, "database" | "databasename") => database = value,
            (Driver::MsSql, "user" | "username" | "user id" | "uid") => user = Some(value),
            (Driver::MsSql, "password" | "pwd") => password = Some(value),
            (Driver::MsSql, "trustservercertificate") => cfg.trust_cert = truthy(Some(&value)),
            (Driver::Postgres | Driver::MySql | Driver::MariaDb, _)
                if tls_param(&mut cfg.tls, &key, &value) => {}
            (Driver::ClickHouse, "secure" | "ssl") => {
                cfg.set_property("tls", if truthy(Some(&value)) { "true" } else { "" })
            }
            _ => extra.push(param.trim().to_string()),
        }
    }
    match cfg.driver {
        Driver::Redis => cfg.set_property(
            "tls",
            if scheme.eq_ignore_ascii_case("rediss://") {
                "true"
            } else {
                ""
            },
        ),
        Driver::MongoDb => cfg.set_property(
            "srv",
            if scheme.eq_ignore_ascii_case("mongodb+srv://") {
                "true"
            } else {
                ""
            },
        ),
        _ => {}
    }
    cfg.url_extra = match cfg.driver {
        _ if extra.is_empty() => String::new(),
        Driver::MsSql => extra.iter().map(|p| format!(";{p}")).collect(),
        _ => format!("?{}", extra.join("&")),
    };
    (cfg.host, cfg.port, cfg.database) = (host, port, database);
    if user.is_some() || password.is_some() {
        if matches!(cfg.auth, AuthMode::NoAuth | AuthMode::AzureCli) {
            cfg.auth = AuthMode::UserPassword;
        }
        cfg.user = user.unwrap_or_default();
    }
    if let Some(p) = password {
        cfg.password = p;
    }
    true
}

/// The SSL parameters of PostgreSQL's and MySQL's URLs (`sslmode=verify-full`, `ssl-mode=REQUIRED`,
/// `sslrootcert=…`, JDBC's `useSSL=false`) into the SSL settings. False for any other parameter.
fn tls_param(t: &mut TlsSettings, key: &str, value: &str) -> bool {
    let file = match key {
        "sslmode" | "ssl-mode" | "ssl_mode" => {
            let mode = match value.to_ascii_lowercase().replace('_', "-").as_str() {
                "disable" | "disabled" => TlsMode::Disable,
                "allow" | "prefer" | "preferred" => TlsMode::Prefer,
                "require" | "required" => TlsMode::Require,
                "verify-ca" => TlsMode::VerifyCa,
                "verify-full" | "verify-identity" => TlsMode::VerifyFull,
                _ => return false,
            };
            t.mode = mode;
            return true;
        }
        "usessl" | "ssl" => {
            match (truthy(Some(value)), t.mode) {
                (false, _) => t.mode = TlsMode::Disable,
                (true, TlsMode::Disable | TlsMode::Prefer) => t.mode = TlsMode::Require,
                _ => {}
            }
            return true;
        }
        "sslrootcert" | "ssl-ca" | "sslca" => &mut t.ca_file,
        "sslcert" | "ssl-cert" => &mut t.cert_file,
        "sslkey" | "ssl-key" => &mut t.key_file,
        _ => return false,
    };
    *file = value.to_string();
    true
}

/// Moves the SSL parameters of a PostgreSQL / MySQL / MariaDB URL's `url_extra` into the SSL
/// settings (DataGrip's imported URLs).
pub(crate) fn take_tls_params(cfg: &mut ConnectionConfig) {
    if !super::tls::supported(cfg.driver) || cfg.url_extra.is_empty() {
        return;
    }
    let extra = std::mem::take(&mut cfg.url_extra);
    let sep = &extra[..1];
    let kept: Vec<&str> = extra[1..]
        .split(['&', ';'])
        .filter(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            !p.trim().is_empty()
                && !tls_param(
                    &mut cfg.tls,
                    &k.trim().to_ascii_lowercase(),
                    &decode(v.trim()),
                )
        })
        .collect();
    if !kept.is_empty() {
        cfg.url_extra = format!("{sep}{}", kept.join(if sep == ";" { ";" } else { "&" }));
    }
}

/// Percent-decoding (`%40` → `@`); a stray `%` is kept.
fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        match (
            b[i],
            b.get(i + 1).copied().and_then(hex),
            b.get(i + 2).copied().and_then(hex),
        ) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (c, ..) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// "Copy as URL": the connection as other tools take it (`postgresql://user@host:5432/db`), never
/// with its password. Engines without such a URL give their JDBC or cluster URL.
pub fn connection_url(cfg: &ConnectionConfig) -> String {
    let host = cfg.host.trim();
    let signs_in = matches!(cfg.auth, AuthMode::UserPassword | AuthMode::Authenticator);
    let user = if signs_in { cfg.user.trim() } else { "" };
    let userinfo = if user.is_empty() {
        String::new()
    } else {
        format!("{}@", enc(user))
    };
    let authority = if cfg.port == 0 {
        host.to_string()
    } else {
        format!("{host}:{}", cfg.port)
    };
    let db = enc(cfg.database.trim());
    let query = |params: Vec<String>| {
        if params.is_empty() {
            String::new()
        } else {
            format!("?{}", params.join("&"))
        }
    };
    match cfg.driver {
        Driver::Postgres | Driver::MySql | Driver::MariaDb => {
            let pg = cfg.driver == Driver::Postgres;
            let t = &cfg.tls;
            let mut params = Vec::new();
            if t.mode != TlsMode::Prefer {
                let mode = match (t.mode, pg) {
                    (TlsMode::Disable, true) => "disable",
                    (TlsMode::Require, true) => "require",
                    (TlsMode::VerifyCa, true) => "verify-ca",
                    (_, true) => "verify-full",
                    (TlsMode::Disable, false) => "DISABLED",
                    (TlsMode::Require, false) => "REQUIRED",
                    (TlsMode::VerifyCa, false) => "VERIFY_CA",
                    (_, false) => "VERIFY_IDENTITY",
                };
                params.push(format!(
                    "{}={mode}",
                    if pg { "sslmode" } else { "ssl-mode" }
                ));
            }
            if t.mode != TlsMode::Disable {
                for (k_pg, k_my, file) in [
                    ("sslrootcert", "ssl-ca", &t.ca_file),
                    ("sslcert", "ssl-cert", &t.cert_file),
                    ("sslkey", "ssl-key", &t.key_file),
                ] {
                    if !file.trim().is_empty() {
                        params.push(format!(
                            "{}={}",
                            if pg { k_pg } else { k_my },
                            enc(file.trim())
                        ));
                    }
                }
            }
            let scheme = match cfg.driver {
                Driver::Postgres => "postgresql",
                Driver::MySql => "mysql",
                _ => "mariadb",
            };
            format!("{scheme}://{userinfo}{authority}/{db}{}", query(params))
        }
        Driver::MsSql => {
            let mut url = format!("sqlserver://{authority}");
            if !db.is_empty() {
                url += &format!(";database={db}");
            }
            if !user.is_empty() {
                url += &format!(";user={}", enc(user));
            }
            if cfg.trust_cert {
                url += ";trustServerCertificate=true";
            }
            url + cfg.url_extra.trim()
        }
        Driver::Redis => {
            let scheme = if truthy(cfg.property("tls")) {
                "rediss"
            } else {
                "redis"
            };
            format!(
                "{scheme}://{userinfo}{authority}/{db}{}",
                cfg.url_extra.trim()
            )
        }
        Driver::MongoDb if mongo_srv(cfg) => format!(
            "mongodb+srv://{userinfo}{host}/{db}{}",
            cfg.url_extra.trim()
        ),
        Driver::MongoDb => format!(
            "mongodb://{userinfo}{authority}/{db}{}",
            cfg.url_extra.trim()
        ),
        Driver::ClickHouse => {
            let extra = cfg.url_extra.trim();
            let secure = if !truthy(cfg.property("tls")) {
                ""
            } else if extra.is_empty() {
                "?secure=true"
            } else {
                "&secure=true"
            };
            format!("clickhouse://{userinfo}{authority}/{db}{extra}{secure}")
        }
        Driver::Sqlite => format!("sqlite:{}", cfg.file_path.trim()),
        Driver::DuckDb => format!("duckdb:{}", cfg.file_path.trim()),
        Driver::Oracle
        | Driver::Snowflake
        | Driver::InfluxDb
        | Driver::Kusto
        | Driver::Prometheus => jdbc_url(cfg),
    }
}

/// `host[:port]`; a missing port is the driver's default, as the JDBC drivers read it. None when
/// the port is out of range (`host:99999`) rather than leaving it inside the host.
pub(crate) fn parse_authority(authority: &str, default_port: u16) -> Option<(String, u16)> {
    match authority.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => {
            let port = p.parse::<u16>().ok().filter(|&p| p != 0)?;
            Some((h.to_string(), port))
        }
        _ => Some((authority.to_string(), default_port)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tls;

    /// The dialog parses the URL field at every keystroke: text whose bytes at a scheme's length
    /// aren't a character boundary is refused, not sliced.
    #[test]
    fn http_schemes_are_matched_on_any_text() {
        for driver in [Driver::InfluxDb, Driver::Kusto, Driver::Prometheus] {
            let mut cfg = ConnectionConfig::new(1, driver);
            let before = cfg.clone();
            for text in ["日本語.example:9090", "é", "ht日p://x", ""] {
                assert!(!apply_jdbc_url(&mut cfg, text), "{driver:?} {text}");
                assert!(!apply_url(&mut cfg, text), "{driver:?} {text}");
            }
            assert_eq!(
                (cfg.host, cfg.port, cfg.properties),
                (before.host, before.port, before.properties)
            );
        }
    }

    #[test]
    fn prometheus_urls_keep_the_path_prefix() {
        let mut cfg = ConnectionConfig::new(1, Driver::Prometheus);
        assert!(apply_jdbc_url(
            &mut cfg,
            "http://vmselect.vm.svc:8481/select/0/prometheus/"
        ));
        assert_eq!(
            (cfg.host.as_str(), cfg.port),
            ("vmselect.vm.svc/select/0/prometheus", 8481)
        );
        assert_eq!(
            jdbc_url(&cfg),
            "http://vmselect.vm.svc:8481/select/0/prometheus"
        );
        assert!(apply_jdbc_url(&mut cfg, "HTTPS://prom.example.com"));
        assert_eq!(
            (cfg.host.as_str(), cfg.port, cfg.property("tls")),
            ("prom.example.com", 443, Some("true"))
        );
        assert_eq!(jdbc_url(&cfg), "https://prom.example.com");
    }

    #[test]
    fn round_trips_every_driver() {
        for driver in Driver::ALL {
            let mut cfg = ConnectionConfig::new(1, driver);
            cfg.host = "db.example.com".into();
            cfg.port = 5555;
            cfg.database = if driver == Driver::Snowflake {
                "ANALYTICS".into()
            } else {
                "sales".into()
            };
            cfg.file_path = "/tmp/x.db".into();
            let url = jdbc_url(&cfg);
            let mut back = ConnectionConfig::new(2, driver);
            assert!(apply_jdbc_url(&mut back, &url), "{url}");
            assert_eq!(jdbc_url(&back), url);
        }
    }

    #[test]
    fn parses_the_usual_forms() {
        let mut cfg = ConnectionConfig::new(1, Driver::Postgres);
        assert!(apply_jdbc_url(
            &mut cfg,
            "jdbc:postgresql://localhost/app?sslmode=require"
        ));
        assert_eq!(
            (cfg.host.as_str(), cfg.port, cfg.database.as_str()),
            ("localhost", 5432, "app")
        );
        assert!(!apply_jdbc_url(&mut cfg, "jdbc:mysql://localhost:3306/app"));

        let mut ms = ConnectionConfig::new(1, Driver::MsSql);
        assert!(apply_jdbc_url(
            &mut ms,
            "jdbc:sqlserver://sql:14330;encrypt=true;databaseName=erp"
        ));
        assert_eq!(
            (ms.host.as_str(), ms.port, ms.database.as_str()),
            ("sql", 14330, "erp")
        );
        assert_eq!(
            jdbc_url(&ms),
            "jdbc:sqlserver://sql:14330;database=erp;encrypt=true"
        );
    }

    /// Parameters without a field survive the dialog (found in review of the Snowflake PR).
    #[test]
    fn keeps_unknown_parameters_and_rejects_bad_ports() {
        let mut pg = ConnectionConfig::new(1, Driver::Postgres);
        let url = "jdbc:postgresql://db:5433/app?sslmode=require&connectTimeout=5";
        assert!(apply_jdbc_url(&mut pg, url));
        assert_eq!(jdbc_url(&pg), url);
        pg.database = "other".into();
        assert_eq!(
            jdbc_url(&pg),
            "jdbc:postgresql://db:5433/other?sslmode=require&connectTimeout=5"
        );

        // An out-of-range port is rejected instead of ending up inside the host.
        let before = pg.clone();
        assert!(!apply_jdbc_url(&mut pg, "jdbc:postgresql://db:99999/app"));
        assert!(!apply_jdbc_url(&mut pg, "jdbc:postgresql://db:0/app"));
        assert_eq!(pg, before);
    }

    #[test]
    fn pasted_connection_urls_fill_the_fields() {
        // A PostgreSQL URL pasted into a new MySQL data source switches the driver.
        let mut cfg = ConnectionConfig::new(1, Driver::MySql);
        cfg.name = "MySQL".into();
        assert!(apply_url(
            &mut cfg,
            "postgresql://app%40corp:p%2Fss@db.example.com:6432/orders?sslmode=verify-full&sslrootcert=%2Fcerts%2Fca.pem&application_name=x"
        ));
        assert_eq!(
            (
                cfg.driver,
                cfg.name.as_str(),
                cfg.host.as_str(),
                cfg.port,
                cfg.database.as_str()
            ),
            (
                Driver::Postgres,
                "PostgreSQL",
                "db.example.com",
                6432,
                "orders"
            )
        );
        assert_eq!(
            (cfg.user.as_str(), cfg.password.as_str()),
            ("app@corp", "p/ss")
        );
        assert_eq!(
            (
                cfg.tls.mode,
                cfg.tls.ca_file.as_str(),
                cfg.url_extra.as_str()
            ),
            (TlsMode::VerifyFull, "/certs/ca.pem", "?application_name=x")
        );

        // Without a password in it, the one typed stays.
        assert!(apply_url(
            &mut cfg,
            "postgres://reader@db.example.com/orders"
        ));
        assert_eq!(
            (cfg.user.as_str(), cfg.password.as_str(), cfg.port),
            ("reader", "p/ss", 5432)
        );

        let mut my = ConnectionConfig::new(1, Driver::Postgres);
        assert!(apply_url(
            &mut my,
            "mysql://root:pw@mysql.example.com/shop?ssl-mode=REQUIRED"
        ));
        assert_eq!(
            (my.driver, my.port, my.database.as_str(), my.tls.mode),
            (Driver::MySql, 3306, "shop", TlsMode::Require)
        );

        let mut ms = ConnectionConfig::new(1, Driver::Postgres);
        assert!(apply_url(
            &mut ms,
            "sqlserver://sql.example.com:1434;database=erp;user=sa;password=Secret1;encrypt=true;trustServerCertificate=false"
        ));
        assert_eq!(
            (
                ms.driver,
                ms.port,
                ms.database.as_str(),
                ms.user.as_str(),
                ms.password.as_str()
            ),
            (Driver::MsSql, 1434, "erp", "sa", "Secret1")
        );
        assert_eq!(
            (ms.trust_cert, ms.url_extra.as_str()),
            (false, ";encrypt=true")
        );

        let mut mongo = ConnectionConfig::new(1, Driver::Postgres);
        assert!(apply_url(
            &mut mongo,
            "mongodb+srv://app:pw@cluster0.example.net/shop?retryWrites=true&w=majority"
        ));
        assert_eq!(
            (
                mongo.driver,
                mongo.host.as_str(),
                mongo.database.as_str(),
                mongo.property("srv")
            ),
            (
                Driver::MongoDb,
                "cluster0.example.net",
                "shop",
                Some("true")
            )
        );
        assert_eq!(
            (mongo.user.as_str(), mongo.url_extra.as_str()),
            ("app", "?retryWrites=true&w=majority")
        );
        assert!(jdbc_url(&mongo).starts_with("mongodb+srv://cluster0.example.net/shop"));

        let mut redis = ConnectionConfig::new(1, Driver::Postgres);
        assert!(apply_url(
            &mut redis,
            "rediss://:secret@cache.example.com:6380/3"
        ));
        assert_eq!(
            (
                redis.driver,
                redis.port,
                redis.database.as_str(),
                redis.user.as_str(),
                redis.password.as_str()
            ),
            (Driver::Redis, 6380, "3", "", "secret")
        );
        assert_eq!(
            (redis.property("tls"), jdbc_url(&redis).as_str()),
            (Some("true"), "rediss://cache.example.com:6380/3")
        );

        let mut ch = ConnectionConfig::new(1, Driver::Postgres);
        assert!(apply_url(
            &mut ch,
            "clickhouse://default@ch.example.com:9440/logs?secure=true"
        ));
        assert_eq!(
            (ch.driver, ch.port, ch.property("tls")),
            (Driver::ClickHouse, 9440, Some("true"))
        );

        for (text, path) in [
            ("sqlite:///data/app.db", "/data/app.db"),
            ("sqlite:////data/app.db", "/data/app.db"),
            ("/data/app.sqlite", "/data/app.sqlite"),
        ] {
            let mut lite = ConnectionConfig::new(1, Driver::Postgres);
            assert!(apply_url(&mut lite, text), "{text}");
            assert_eq!(
                (lite.driver, lite.file_path.as_str()),
                (Driver::Sqlite, path)
            );
        }

        // Neither a URL nor this driver's JDBC URL: nothing changes.
        let before = cfg.clone();
        assert!(!apply_url(&mut cfg, "ftp://host/file"));
        assert!(!apply_url(&mut cfg, "postgresql://"));
        assert_eq!(cfg, before);
    }

    #[test]
    fn copy_as_url_never_has_the_password_and_pastes_back() {
        for driver in Driver::ALL {
            let mut cfg = ConnectionConfig::new(1, driver);
            cfg.host = "db.example.com".into();
            cfg.port = 5555;
            cfg.database = if driver == Driver::Snowflake {
                "ANALYTICS".into()
            } else {
                "sales".into()
            };
            cfg.user = "app user".into();
            cfg.password = "SECRET-PW".into();
            cfg.file_path = "/tmp/x.db".into();
            cfg.tls.mode = TlsMode::Require;
            let url = connection_url(&cfg);
            assert!(!url.contains("SECRET-PW"), "{url}");
            let mut back = ConnectionConfig::new(2, Driver::Postgres);
            assert!(
                apply_url(&mut back, &url)
                    || matches!(
                        driver,
                        Driver::Oracle
                            | Driver::Snowflake
                            | Driver::InfluxDb
                            | Driver::Kusto
                            | Driver::Prometheus
                    ),
                "{url}"
            );
            if matches!(
                driver,
                Driver::Postgres
                    | Driver::MySql
                    | Driver::MariaDb
                    | Driver::MsSql
                    | Driver::Redis
                    | Driver::MongoDb
                    | Driver::ClickHouse
            ) {
                assert_eq!(
                    (
                        back.driver,
                        back.host.as_str(),
                        back.port,
                        back.database.as_str(),
                        back.user.as_str()
                    ),
                    (driver, "db.example.com", 5555, "sales", "app user"),
                    "{url}"
                );
            }
            if tls::supported(driver) {
                assert_eq!(back.tls.mode, TlsMode::Require, "{url}");
            }
        }
        let mut pg = ConnectionConfig::new(1, Driver::Postgres);
        pg.host = "db".into();
        assert_eq!(
            connection_url(&pg),
            "postgresql://postgres@db:5432/postgres"
        );
    }
}
