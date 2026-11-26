//! SAP HANA driver.

use super::{ConnectionConfig, ResultSet, SchemaInfo};

/// An open SAP HANA session.
pub struct HanaSession {}

impl HanaSession {
    pub async fn connect(_cfg: &ConnectionConfig) -> anyhow::Result<Self> {
        anyhow::bail!("SAP HANA is not implemented yet")
    }

    /// Runs ONE statement; at most `max_rows` rows per result (0 = discard rows).
    pub async fn execute(
        &mut self,
        _sql: &str,
        _max_rows: usize,
    ) -> anyhow::Result<Vec<ResultSet>> {
        anyhow::bail!("SAP HANA is not implemented yet")
    }

    /// A SELECT with `?` parameters bound as text.
    pub async fn execute_bound(
        &mut self,
        _sql: &str,
        _parameters: &[String],
        _max_rows: usize,
    ) -> anyhow::Result<Vec<ResultSet>> {
        anyhow::bail!("SAP HANA is not implemented yet")
    }

    pub async fn current_database(&mut self) -> anyhow::Result<String> {
        anyhow::bail!("SAP HANA is not implemented yet")
    }

    pub async fn list_databases(&mut self) -> anyhow::Result<Vec<String>> {
        anyhow::bail!("SAP HANA is not implemented yet")
    }

    /// Data editor changes in ONE transaction; each statement must affect exactly one row.
    pub async fn apply_changes(&mut self, _statements: &[String]) -> anyhow::Result<u64> {
        anyhow::bail!("SAP HANA is not implemented yet")
    }

    /// Schemas, tables/views and columns, without system objects.
    pub async fn introspect(&mut self) -> anyhow::Result<Vec<SchemaInfo>> {
        anyhow::bail!("SAP HANA is not implemented yet")
    }

    /// Product and version for Test Connection.
    pub async fn server_version(&mut self) -> Option<String> {
        None
    }

    /// Runs the statements in ONE transaction (all or nothing); the affected rows.
    pub async fn run_transaction(&mut self, _statements: &[String]) -> anyhow::Result<u64> {
        anyhow::bail!("SAP HANA is not implemented yet")
    }

    /// Manual transactions (console): from here on nothing commits by itself until
    /// `end_transaction`.
    pub async fn begin(&mut self) -> anyhow::Result<()> {
        anyhow::bail!("SAP HANA is not implemented yet")
    }

    /// Commits or rolls back the Manual transaction and goes back to autocommit.
    pub async fn end_transaction(&mut self, _commit: bool) -> anyhow::Result<()> {
        anyhow::bail!("SAP HANA is not implemented yet")
    }

    pub fn is_closed(&self) -> bool {
        false
    }
}
