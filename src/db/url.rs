//! The connection dialog's URL field: DataGrip's JDBC URL for each driver, built from the
//! General tab fields and parsed back into them when the URL is edited.

use super::{ConnectionConfig, Driver, snowflake};

/// The URL DataGrip shows for these settings (default URL templates of its drivers), followed by
/// the parameters that have no field (`url_extra`).
pub fn jdbc_url(cfg: &ConnectionConfig) -> String {
    let host = cfg.host.trim();
    let db = cfg.database.trim();
    let extra = cfg.url_extra.trim();
    let authority = if cfg.port == 0 { host.to_string() } else { format!("{host}:{}", cfg.port) };
    let slash_db = if db.is_empty() { String::new() } else { format!("/{db}") };
    match cfg.driver {
        Driver::Postgres => format!("jdbc:postgresql://{authority}{slash_db}{extra}"),
        Driver::MySql => format!("jdbc:mysql://{authority}{slash_db}{extra}"),
        Driver::MariaDb => format!("jdbc:mariadb://{authority}{slash_db}{extra}"),
        Driver::MsSql if db.is_empty() => format!("jdbc:sqlserver://{authority}{extra}"),
        Driver::MsSql => format!("jdbc:sqlserver://{authority};database={db}{extra}"),
        Driver::Oracle => format!("jdbc:oracle:thin:@//{authority}{slash_db}{extra}"),
        Driver::Sqlite => format!("jdbc:sqlite:{}", cfg.file_path.trim()),
        Driver::Snowflake => snowflake::jdbc_url(cfg),
        Driver::Redis => format!("redis://{authority}{slash_db}{extra}"),
        Driver::MongoDb => format!("mongodb://{authority}{slash_db}{extra}"),
        Driver::ClickHouse => format!("clickhouse://{authority}{slash_db}{extra}"),
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
            let Some(path) = url.strip_prefix("jdbc:sqlite:") else { return false };
            cfg.file_path = path.to_string();
            return true;
        }
        Driver::Snowflake => return snowflake::apply_jdbc_url(cfg, url),
        Driver::Redis => "redis://",
        Driver::MongoDb => "mongodb://",
        Driver::ClickHouse => "clickhouse://",
    };
    let Some(rest) = url.strip_prefix(prefix) else { return false };
    let default_port = cfg.driver.default_port();
    if cfg.driver == Driver::MsSql {
        // host:port;database=db;other=…
        let mut parts = rest.split(';');
        let Some((host, port)) = parse_authority(parts.next().unwrap_or(""), default_port) else { return false };
        let (mut database, mut extra) = (String::new(), String::new());
        for part in parts.filter(|p| !p.trim().is_empty()) {
            match part.split_once('=') {
                Some((k, v)) if k.trim().eq_ignore_ascii_case("database") || k.trim().eq_ignore_ascii_case("databaseName") => {
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
    let Some((host, port)) = parse_authority(authority, default_port) else { return false };
    (cfg.host, cfg.port, cfg.database, cfg.url_extra) = (host, port, db.to_string(), extra.to_string());
    true
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

    #[test]
    fn round_trips_every_driver() {
        for driver in Driver::ALL {
            let mut cfg = ConnectionConfig::new(1, driver);
            cfg.host = "db.example.com".into();
            cfg.port = 5555;
            cfg.database = if driver == Driver::Snowflake { "ANALYTICS".into() } else { "sales".into() };
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
        assert!(apply_jdbc_url(&mut cfg, "jdbc:postgresql://localhost/app?sslmode=require"));
        assert_eq!((cfg.host.as_str(), cfg.port, cfg.database.as_str()), ("localhost", 5432, "app"));
        assert!(!apply_jdbc_url(&mut cfg, "jdbc:mysql://localhost:3306/app"));

        let mut ms = ConnectionConfig::new(1, Driver::MsSql);
        assert!(apply_jdbc_url(&mut ms, "jdbc:sqlserver://sql:14330;encrypt=true;databaseName=erp"));
        assert_eq!((ms.host.as_str(), ms.port, ms.database.as_str()), ("sql", 14330, "erp"));
        assert_eq!(jdbc_url(&ms), "jdbc:sqlserver://sql:14330;database=erp;encrypt=true");
    }

    /// Parameters without a field survive the dialog (found in review of the Snowflake PR).
    #[test]
    fn keeps_unknown_parameters_and_rejects_bad_ports() {
        let mut pg = ConnectionConfig::new(1, Driver::Postgres);
        let url = "jdbc:postgresql://db:5433/app?sslmode=require&connectTimeout=5";
        assert!(apply_jdbc_url(&mut pg, url));
        assert_eq!(jdbc_url(&pg), url);
        pg.database = "other".into();
        assert_eq!(jdbc_url(&pg), "jdbc:postgresql://db:5433/other?sslmode=require&connectTimeout=5");

        // An out-of-range port is rejected instead of ending up inside the host.
        let before = pg.clone();
        assert!(!apply_jdbc_url(&mut pg, "jdbc:postgresql://db:99999/app"));
        assert!(!apply_jdbc_url(&mut pg, "jdbc:postgresql://db:0/app"));
        assert_eq!(pg, before);
    }
}
