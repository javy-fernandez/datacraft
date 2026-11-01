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
        ui.add_enabled_ui(writable && table && crate::db::table_definition::supported(cx.driver), |ui| {
            ui.menu_button(tr("Duplicate Table"), |ui| {
                for (data, label) in [(true, "Structure and Data"), (false, "Structure Only")] {
                    if ui.button(tr(label)).clicked() {
                        if let Some(table) = tables.first() { actions.push(ExplorerAction::DuplicateTable(table.clone(), data)); }
                        ui.close();
                    }
                }
            });
        });
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
    if sql
        && ui
            .add_enabled(
                cx.connected && kind == Some(ObjectKind::Table) && crate::db::table_model::supported(cx.driver),
                egui::Button::new(tr("Reverse Tables to Model…")),
            )
            .on_disabled_hover_text(tr("Not available for this engine"))
            .clicked()
    {
        actions.push(ExplorerAction::TableModel(tables.to_vec()));
        ui.close();
    }
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
