//! Confirmation dialog to drop a database, table or view, truncate a table or rename it,
//! showing the SQL that will run.

use crate::db::{Driver, ObjectKind};
use crate::i18n::{tr, trf};
use crate::sql;
use crate::ui::quote_ident_if_needed;
use crate::ui::theme::{self, palette};
use crate::worker::ConnId;
use egui::{FontId, Margin, RichText, vec2};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DropKind {
    Database,
    Table,
    View,
    /// Delete every row of the table (TRUNCATE).
    Truncate,
    /// Rename the table to `new_name`.
    Rename,
}

impl From<ObjectKind> for DropKind {
    fn from(k: ObjectKind) -> Self {
        match k {
            ObjectKind::Table => DropKind::Table,
            ObjectKind::View => DropKind::View,
        }
    }
}

pub struct DropDialog {
    pub conn: ConnId,
    pub driver: Driver,
    pub kind: DropKind,
    /// Database being dropped, or the one holding the table/view.
    pub db: String,
    /// Schema of the table/view (empty for databases).
    pub schema: String,
    /// Name of the table/view (for databases, same as `db`).
    pub name: String,
    /// Databases must be confirmed by typing their name.
    confirm: String,
    /// New name (`Rename`).
    pub new_name: String,
    focus_name: bool,
    pub running: bool,
    pub error: Option<String>,
}

pub enum DropResult {
    Drop(Vec<String>),
    Cancel,
}

#[cfg(test)]
pub fn drop_sql(driver: Driver, kind: DropKind, schema: &str, name: &str) -> String {
    object_sql(driver, kind, schema, name, "")
}

/// Name the table will have after a rename. Oracle and Snowflake: a simple name typed in
/// lowercase means the usual uppercase name, not a case-sensitive quoted one.
pub fn effective_name(driver: Driver, new_name: &str) -> String {
    let n = new_name.trim();
    let simple = n.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if matches!(driver, Driver::Oracle | Driver::Snowflake) && simple { n.to_ascii_uppercase() } else { n.to_string() }
}

/// Statement for `kind` on `schema.name`; `new_name` is only used by `Rename`.
pub fn object_sql(driver: Driver, kind: DropKind, schema: &str, name: &str, new_name: &str) -> String {
    let q = |s: &str| quote_ident_if_needed(driver, s);
    let object = |s: &str, n: &str| if driver == Driver::Sqlite || s.is_empty() { q(n) } else { format!("{}.{}", q(s), q(n)) };
    match kind {
        DropKind::Database => format!("drop database {}", q(name)),
        DropKind::Table => format!("drop table {}", object(schema, name)),
        DropKind::View => format!("drop view {}", object(schema, name)),
        // SQLite has no TRUNCATE.
        DropKind::Truncate if driver == Driver::Sqlite => format!("delete from {}", object(schema, name)),
        DropKind::Truncate => format!("truncate table {}", object(schema, name)),
        DropKind::Rename => {
            let new_name = &effective_name(driver, new_name);
            match driver {
                Driver::MsSql => {
                    let old = format!("{}.{}", q(schema), q(name)).replace('\'', "''");
                    format!("exec sp_rename '{old}', '{}'", new_name.replace('\'', "''"))
                }
                Driver::MySql | Driver::MariaDb | Driver::ClickHouse => format!("rename table {} to {}", object(schema, name), object(schema, new_name)),
                // Snowflake resolves an unqualified new name in the session's current schema.
                Driver::Snowflake => format!("alter table {} rename to {}", object(schema, name), object(schema, new_name)),
                // The new name can't be qualified on PostgreSQL, SQLite or Oracle.
                _ => format!("alter table {} rename to {}", object(schema, name), q(new_name)),
            }
        }
    }
}

impl DropDialog {
    pub fn database(conn: ConnId, driver: Driver, db: String) -> Self {
        Self::new(conn, driver, DropKind::Database, db.clone(), String::new(), db)
    }

    pub fn object(conn: ConnId, driver: Driver, kind: DropKind, db: String, schema: String, name: String) -> Self {
        Self::new(conn, driver, kind, db, schema, name)
    }

    fn new(conn: ConnId, driver: Driver, kind: DropKind, db: String, schema: String, name: String) -> Self {
        let new_name = if kind == DropKind::Rename { name.clone() } else { String::new() };
        Self { conn, driver, kind, db, schema, name, confirm: String::new(), new_name, focus_name: true, running: false, error: None }
    }

    pub fn sql(&self) -> String {
        object_sql(self.driver, self.kind, &self.schema, &self.name, &self.new_name)
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Option<DropResult> {
        let mut result = None;
        let p = palette(ctx.global_style().visuals.dark_mode);
        let dark = ctx.global_style().visuals.dark_mode;
        let frame = egui::Frame::popup(&ctx.global_style())
            .fill(p.popup_bg)
            .corner_radius(12)
            .inner_margin(Margin::same(20))
            .stroke(egui::Stroke::new(1.0, p.popup_border));
        let (title, question) = match self.kind {
            DropKind::Database => (tr("Drop Database"), trf("Drop database “{name}” and all its data?", &[("name", &self.name)])),
            DropKind::Table => (tr("Drop Table"), trf("Drop table “{name}” and all its data?", &[("name", &self.name)])),
            DropKind::View => (tr("Drop View"), trf("Drop view “{name}”?", &[("name", &self.name)])),
            DropKind::Truncate => (tr("Truncate Table"), trf("Delete every row of table “{name}”?", &[("name", &self.name)])),
            DropKind::Rename => (tr("Rename Table"), trf("Rename table “{name}” to:", &[("name", &self.name)])),
        };
        let rename = self.kind == DropKind::Rename;
        let needs_typing = self.kind == DropKind::Database;
        egui::Modal::new(egui::Id::new("drop_dialog")).frame(frame).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.label(RichText::new(title).font(theme::bold(15.0)).color(p.text));
            ui.add_space(8.0);
            ui.label(question);
            if rename {
                ui.add_space(4.0);
                let name = ui.add_enabled(
                    !self.running,
                    egui::TextEdit::singleline(&mut self.new_name).margin(vec2(8.0, 5.0)).desired_width(f32::INFINITY),
                );
                if std::mem::take(&mut self.focus_name) {
                    name.request_focus();
                }
            } else {
                ui.label(RichText::new(tr("This action cannot be undone.")).color(p.error));
            }
            ui.add_space(10.0);
            egui::Frame::NONE
                .fill(p.editor_bg)
                .stroke(egui::Stroke::new(1.0, p.control_border))
                .corner_radius(6)
                .inner_margin(Margin::same(8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let mut job = sql::highlight(&format!("{};", self.sql()), dark, FontId::monospace(12.5));
                    job.wrap.max_width = ui.available_width();
                    ui.label(job);
                });
            if needs_typing {
                ui.add_space(10.0);
                ui.label(trf("Type “{name}” to confirm:", &[("name", &self.name)]));
                ui.add_space(4.0);
                ui.add_enabled(
                    !self.running,
                    egui::TextEdit::singleline(&mut self.confirm).margin(vec2(8.0, 5.0)).desired_width(f32::INFINITY),
                );
            }
            if let Some(e) = &self.error {
                ui.add_space(8.0);
                ui.add(egui::Label::new(RichText::new(e).monospace().color(p.error)).wrap());
            }
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                if self.running {
                    ui.add(egui::Spinner::new().size(14.0).color(p.muted));
                    let busy = match self.kind {
                        DropKind::Rename => tr("Renaming…"),
                        DropKind::Truncate => tr("Truncating…"),
                        _ => tr("Dropping…"),
                    };
                    ui.label(RichText::new(busy).color(p.muted));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let new_name = self.new_name.trim();
                    let confirmed = if rename { !new_name.is_empty() && new_name != self.name } else { !needs_typing || self.confirm == self.name };
                    let label = match self.kind {
                        DropKind::Truncate => tr("Truncate"),
                        DropKind::Rename => tr("Rename"),
                        _ => tr("Drop"),
                    };
                    let enter = rename && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.add_enabled_ui(!self.running && confirmed, |ui| {
                        if theme::primary_button(ui, label).clicked() || (enter && !self.running && confirmed) {
                            result = Some(DropResult::Drop(vec![self.sql()]));
                        }
                    });
                    ui.add_enabled_ui(!self.running, |ui| {
                        if theme::secondary_button(ui, tr("Cancel")).clicked() {
                            result = Some(DropResult::Cancel);
                        }
                    });
                });
            });
        });
        if result.is_none() && !self.running && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            result = Some(DropResult::Cancel);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_sql_per_dialect() {
        assert_eq!(drop_sql(Driver::Postgres, DropKind::Database, "", "Shop"), "drop database \"Shop\"");
        assert_eq!(drop_sql(Driver::MsSql, DropKind::Table, "dbo", "Order"), "drop table dbo.[Order]");
        assert_eq!(drop_sql(Driver::Postgres, DropKind::View, "public", "v1"), "drop view public.v1");
        assert_eq!(drop_sql(Driver::Sqlite, DropKind::Table, "main", "items"), "drop table items");
        assert_eq!(drop_sql(Driver::MySql, DropKind::Table, "shop", "Order"), "drop table shop.`Order`");
    }

    #[test]
    fn truncate_and_rename_per_dialect() {
        let t = |d, k, n| object_sql(d, k, "s", "items", n);
        assert_eq!(t(Driver::Postgres, DropKind::Truncate, ""), "truncate table s.items");
        assert_eq!(t(Driver::Sqlite, DropKind::Truncate, ""), "delete from items");
        assert_eq!(t(Driver::Postgres, DropKind::Rename, " goods "), "alter table s.items rename to goods");
        assert_eq!(t(Driver::Sqlite, DropKind::Rename, "goods"), "alter table items rename to goods");
        assert_eq!(t(Driver::MySql, DropKind::Rename, "goods"), "rename table s.items to s.goods");
        assert_eq!(t(Driver::ClickHouse, DropKind::Rename, "goods"), "rename table s.items to s.goods");
        assert_eq!(t(Driver::MsSql, DropKind::Rename, "it's"), "exec sp_rename 's.items', 'it''s'");
        assert_eq!(object_sql(Driver::Oracle, DropKind::Rename, "HR", "ITEMS", "GOODS"), "alter table HR.ITEMS rename to GOODS");
        assert_eq!(object_sql(Driver::Oracle, DropKind::Rename, "HR", "ITEMS", "goods"), "alter table HR.ITEMS rename to GOODS");
        assert_eq!(object_sql(Driver::Oracle, DropKind::Rename, "HR", "ITEMS", "_x"), "alter table HR.ITEMS rename to \"_x\"");
        assert_eq!(object_sql(Driver::Snowflake, DropKind::Rename, "PUBLIC", "ITEMS", "goods"), "alter table PUBLIC.ITEMS rename to PUBLIC.GOODS");
        assert_eq!(object_sql(Driver::Snowflake, DropKind::Rename, "PUBLIC", "items", "Goods"), "alter table PUBLIC.\"items\" rename to PUBLIC.GOODS");
    }
}
