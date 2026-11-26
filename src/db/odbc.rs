//! ODBC driver.

use super::{ConnectionConfig, ResultSet, SchemaInfo};

/// An open ODBC session.
pub struct OdbcSession {}

impl OdbcSession {
    pub async fn connect(_cfg: &ConnectionConfig) -> anyhow::Result<Self> {
        anyhow::bail!("ODBC is not implemented yet")
    }

    /// Runs ONE statement; at most `max_rows` rows per result (0 = discard rows).
    pub async fn execute(
        &mut self,
        _sql: &str,
        _max_rows: usize,
    ) -> anyhow::Result<Vec<ResultSet>> {
        anyhow::bail!("ODBC is not implemented yet")
    }

    /// A SELECT with `?` parameters bound as text.
    pub async fn execute_bound(
        &mut self,
        _sql: &str,
        _parameters: &[String],
        _max_rows: usize,
    ) -> anyhow::Result<Vec<ResultSet>> {
        anyhow::bail!("ODBC is not implemented yet")
    }

    pub async fn current_database(&mut self) -> anyhow::Result<String> {
        anyhow::bail!("ODBC is not implemented yet")
    }

    pub async fn list_databases(&mut self) -> anyhow::Result<Vec<String>> {
        anyhow::bail!("ODBC is not implemented yet")
    }

    /// Data editor changes in ONE transaction; each statement must affect exactly one row.
    pub async fn apply_changes(&mut self, _statements: &[String]) -> anyhow::Result<u64> {
        anyhow::bail!("ODBC is not implemented yet")
    }

    /// Schemas, tables/views and columns, without system objects.
    pub async fn introspect(&mut self) -> anyhow::Result<Vec<SchemaInfo>> {
        anyhow::bail!("ODBC is not implemented yet")
    }

    /// Product and version for Test Connection.
    pub async fn server_version(&mut self) -> Option<String> {
        None
    }

    /// Runs the statements in ONE transaction (all or nothing); the affected rows.
    pub async fn run_transaction(&mut self, _statements: &[String]) -> anyhow::Result<u64> {
        anyhow::bail!("ODBC is not implemented yet")
    }

    /// Manual transactions (console): from here on nothing commits by itself until
    /// `end_transaction`.
    pub async fn begin(&mut self) -> anyhow::Result<()> {
        anyhow::bail!("ODBC is not implemented yet")
    }

    /// Commits or rolls back the Manual transaction and goes back to autocommit.
    pub async fn end_transaction(&mut self, _commit: bool) -> anyhow::Result<()> {
        anyhow::bail!("ODBC is not implemented yet")
    }

    pub fn is_closed(&self) -> bool {
        false
    }
}

/// The ODBC connection string: the DSN (`DSN=name`) or, when the database field already holds
/// a full string (`DRIVER={…};HOST=…`), that string; user and password are added by `connect`.
pub fn connection_string(cfg: &ConnectionConfig) -> String {
    cfg.database.trim().to_string()
}

/// `odbc:<DSN or connection string>`, as `connection_string` writes it. False when the URL isn't
/// an ODBC one.
pub fn apply_url(cfg: &mut ConnectionConfig, url: &str) -> bool {
    let Some(rest) = url.trim().strip_prefix("odbc:") else {
        return false;
    };
    cfg.database = rest.trim().to_string();
    true
}

/// Connection string of Progress' OpenEdge ODBC driver: the driver's name as installed (it
/// changes with the version), the SQL broker's host and port, the database and its SQL schema.
pub const OPENEDGE_TEMPLATE: &str = "DRIVER={Progress OpenEdge 12.8 Driver};HOST=localhost;PORT=20931;DB=sports2020;DefaultSchema=PUB";
