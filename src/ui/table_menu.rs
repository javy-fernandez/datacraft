//! Shared table actions for the explorer tree and Objects list/grid.

use crate::db::{Driver, ObjectKind};
use crate::i18n::tr;
use crate::ui::explorer::{ExplorerAction, TableRef};

#[derive(Clone, Copy)]
pub struct TableContext {
    pub driver: Driver,
    pub connected: bool,
    pub read_only: bool,
}

pub fn show(ui: &mut egui::Ui, tables: &[TableRef], kind: Option<ObjectKind>, cx: TableContext, actions: &mut Vec<ExplorerAction>) {
    let sql = !cx.driver.is_nosql();
    let single = tables.len() == 1 && kind.is_some();
    let writable = cx.connected && !cx.read_only && sql;
    let table = single && kind == Some(ObjectKind::Table);
    let data_export = crate::db::table_export::supported(cx.driver);
    let structure_export = crate::db::table_export::supported_kind(cx.driver, crate::db::table_export::ExportKind::Sql { structure: true, data: false });
    macro_rules! button {
        ($ui:expr, $enabled:expr, $label:expr, $action:expr) => {{
            if $ui.add_enabled($enabled, egui::Button::new(tr($label))).clicked() {
                if let Some(t) = tables.first() { actions.push($action(t)); }
                $ui.close();
            }
        }};
        ($enabled:expr, $label:expr, $action:expr) => {{
            let response = ui.add_enabled($enabled, egui::Button::new(tr($label)));
            let response = if !$enabled {
                response.on_disabled_hover_text(if !cx.connected {
                    tr("The connection is not open")
                } else if cx.read_only {
                    tr("Read-only connection")
                } else {
                    tr("Select one supported table")
                })
            } else {
                response
            };
            if response.clicked() {
                if let Some(t) = tables.first() {
                    actions.push($action(t));
                }
                ui.close();
            }
        }};
    }
    if ui.add_enabled(cx.connected, egui::Button::new(tr("Open data"))).clicked() {
        for t in tables {
            actions.push(ExplorerAction::OpenTable(t.clone()));
        }
        ui.close();
    }
    if sql {
        button!(writable && table, "Modify Table…", &|t: &TableRef| ExplorerAction::ModifyTable(t.clone()));
        let grants = writable && table && crate::db::table_privileges::supported(cx.driver);
        button!(grants, "Set Privileges…", &|t: &TableRef| ExplorerAction::TablePrivileges(t.clone()));
        if kind == Some(ObjectKind::View) {
            button!(writable && single, "Edit View…", &|t: &TableRef| ExplorerAction::EditView(t.clone()));
        }
        button!(writable && single, "New Table…", &|t: &TableRef| ExplorerAction::NewTable {
            conn: t.conn,
            db: t.db.clone(),
            schema: Some(t.schema.clone())
        });
        button!(writable && single, "New View…", &|t: &TableRef| ExplorerAction::NewView { conn: t.conn, db: t.db.clone(), schema: Some(t.schema.clone()) });
        ui.separator();
        button!(writable && table, "Empty Table…", &|t: &TableRef| ExplorerAction::EmptyTable(t.clone()));
        button!(writable && table, "Truncate…", &|t: &TableRef| ExplorerAction::TruncateTable(t.clone()));
        button!(writable && table, "Import Data from File…", &|t: &TableRef| ExplorerAction::ImportData(t.clone()));
        button!(cx.connected && table && data_export, "Export Table…", &|t: &TableRef| ExplorerAction::ExportTable(t.clone()));
        ui.add_enabled_ui(cx.connected && table && (data_export || structure_export), |ui| {
            ui.menu_button(tr("Dump SQL File"), |ui| {
                button!(ui, data_export, "Structure and Data", &|t: &TableRef| ExplorerAction::DumpTable(t.clone(), true, true));
                button!(ui, structure_export, "Structure Only", &|t: &TableRef| ExplorerAction::DumpTable(t.clone(), true, false));
                button!(ui, data_export, "Data Only", &|t: &TableRef| ExplorerAction::DumpTable(t.clone(), false, true));
            });
        });
        button!(cx.connected && table, "Print Table…", &|t: &TableRef| ExplorerAction::PrintTable(t.clone()));
        ui.separator();
        button!(cx.connected && single, "Generate SELECT", &|t: &TableRef| ExplorerAction::GenerateSelect(t.clone()));
        button!(cx.connected && table, "Generate INSERT", &|t: &TableRef| ExplorerAction::GenerateInsert(t.clone()));
    }
    button!(single, "Show DDL", &|t: &TableRef| ExplorerAction::GenerateDdl(t.clone()));
    if cx.driver.can_copy() {
        button!(cx.connected && single, "Copy Table To…", &|t: &TableRef| ExplorerAction::CopyTable(t.clone()));
    }
    if ui.button(tr("Copy Name")).clicked() {
        ui.ctx().copy_text(tables.iter().map(|t| t.table.as_str()).collect::<Vec<_>>().join("\n"));
        ui.close();
    }
    if sql {
        button!(writable && table, "Rename…", &|t: &TableRef| ExplorerAction::RenameTable(t.clone()));
    }
    ui.separator();
    button!(cx.connected && single, "Refresh", &|t: &TableRef| ExplorerAction::RefreshObject(t.clone()));
    button!(cx.connected && single, "Object Information", &|t: &TableRef| ExplorerAction::ObjectInformation(t.clone()));
    if sql {
        ui.separator();
        button!(writable && single, "Drop…", &|t: &TableRef| ExplorerAction::DropObject(t.clone(), kind.unwrap_or(ObjectKind::Table)));
    }
}

#[cfg(test)]
mod grants_tests {
    use super::*;
    use egui_kittest::kittest::{NodeT, Queryable};

    #[test]
    fn grants_menu_is_available_only_for_one_writable_server_table() {
        let table = TableRef { conn: 7, db: "main".into(), schema: "dbo".into(), table: "notes".into() };
        for (driver, connected, read_only, kind, count, enabled) in [
            (Driver::MsSql, true, false, ObjectKind::Table, 1, true),
            (Driver::Postgres, false, false, ObjectKind::Table, 1, false),
            (Driver::MySql, true, true, ObjectKind::Table, 1, false),
            (Driver::Oracle, true, false, ObjectKind::View, 1, false),
            (Driver::Snowflake, true, false, ObjectKind::Table, 2, false),
            (Driver::Sqlite, true, false, ObjectKind::Table, 1, false),
        ] {
            let tables = vec![table.clone(); count];
            let mut harness = egui_kittest::Harness::builder().with_size(egui::vec2(600.0, 900.0)).build_ui_state(
                move |ui, actions: &mut Vec<ExplorerAction>| show(ui, &tables, Some(kind), TableContext { driver, connected, read_only }, actions),
                Vec::new(),
            );
            harness.run_steps(2);
            let button = harness.get_by_label("Set Privileges…");
            assert_eq!(!button.accesskit_node().is_disabled(), enabled);
            if enabled {
                button.click();
                harness.run_steps(2);
                assert_eq!(harness.state().len(), 1);
            }
        }
    }
}
