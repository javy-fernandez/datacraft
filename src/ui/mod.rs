//! User interface components.

pub mod connection_dialog;
pub mod backup_dialog;
pub mod branding;
pub mod window_chrome;
pub(crate) mod workflow_dialog;
pub mod copy_dialog;
pub mod console;
pub mod create_dialog;
pub mod drop_dialog;
pub mod import_dialog;
pub mod inspector;
pub mod mask_export_dialog;
pub mod modify_table_dialog;
pub mod editing;
pub mod filter_sort;
pub mod explorer;
pub mod icons;
pub mod object_browser;
pub mod table_menu;
pub mod object_organization;
pub mod performance;
pub mod plan_view;
pub mod results;
pub mod routine_dialog;
pub mod routine_execute;
pub mod schema_search;
pub mod script_dialog;
pub mod theme;
pub mod view_dialog;
pub mod user_dialog;

use crate::db::Driver;

/// Summarized connection status, for components that do not need the details.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConnStatusKind {
    Disconnected,
    Connecting,
    Connected,
    Error,
}

/// Connection summary for the console selectors.
#[derive(Clone, Debug)]
pub struct ConnSummary {
    pub id: crate::worker::ConnId,
    pub name: String,
    pub driver: Driver,
    pub status: ConnStatusKind,
    /// Known databases (empty if not connected).
    pub databases: Vec<String>,
    /// Default database of the connection (if connected).
    pub default_db: Option<String>,
}

impl ConnSummary {
    /// "connection · database" label (SQLite only has "main": it is omitted).
    pub fn label(&self, db: Option<&str>) -> String {
        match db.or(self.default_db.as_deref()) {
            Some(d) if self.driver != Driver::Sqlite => format!("{} · {d}", self.name),
            _ => self.name.clone(),
        }
    }
}

/// Char index -> byte index (clamped to the end of the text).
pub fn char_to_byte(text: &str, char_idx: usize) -> usize {
    text.char_indices().nth(char_idx).map(|(b, _)| b).unwrap_or(text.len())
}

/// Byte index -> char index.
pub fn byte_to_char(text: &str, byte_idx: usize) -> usize {
    let b = byte_idx.min(text.len());
    text.char_indices().take_while(|(i, _)| *i < b).count()
}

/// Quotes an identifier for the dialect.
pub fn quote_ident(driver: Driver, name: &str) -> String {
    driver.quote_ident(name)
}

/// Quotes only when needed (simple lowercase identifiers are left as is).
pub fn quote_ident_if_needed(driver: Driver, name: &str) -> String {
    let simple = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && (driver != Driver::Postgres || !name.chars().any(|c| c.is_ascii_uppercase()))
        // Oracle folds unquoted names to uppercase and they must start with a letter; Snowflake
        // also folds them to uppercase.
        && (driver != Driver::Oracle || (!name.chars().any(|c| c.is_ascii_lowercase()) && !name.starts_with('_')))
        && (driver != Driver::Snowflake || !name.chars().any(|c| c.is_ascii_lowercase()))
        && !crate::sql::KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(name));
    if simple { name.to_string() } else { quote_ident(driver, name) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_byte_roundtrip() {
        let t = "añb€c";
        assert_eq!(char_to_byte(t, 0), 0);
        assert_eq!(char_to_byte(t, 2), 3);
        assert_eq!(char_to_byte(t, 4), 7);
        assert_eq!(char_to_byte(t, 99), t.len());
        assert_eq!(byte_to_char(t, 7), 4);
        assert_eq!(byte_to_char(t, t.len()), 5);
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_ident(Driver::MsSql, "a]b"), "[a]]b]");
        assert_eq!(quote_ident(Driver::Postgres, "a\"b"), "\"a\"\"b\"");
    }
}

pub mod table_model;
