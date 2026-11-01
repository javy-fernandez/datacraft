//! Complete table exports and printable reports, independent of data editor pages.

use super::*;
use crate::db::table_export::{self, ExportKind, ExportProgress, ExportRequest};
use std::{
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
};

#[derive(Default)]
pub(super) struct ExportState {
    dialog: Option<Dialog>,
}

impl ExportState {
    pub(super) fn is_open(&self) -> bool {
        self.dialog.is_some()
    }
}

struct Dialog {
    id: u64,
    table: TableRef,
    driver: Driver,
    kind: ExportKind,
    path: String,
    overwrite: bool,
    pending: bool,
    progress: Arc<ExportProgress>,
    error: Option<String>,
    success: Option<String>,
}

enum Intent {
    Export,
    Close,
}

impl Dialog {
    fn title(&self) -> &'static str {
        tr(match self.kind {
            ExportKind::Print { .. } => "Print Table",
            ExportKind::Sql { .. } => "Dump SQL File",
            _ => "Export Table",
        })
    }

    fn show(&mut self, ctx: &egui::Context) -> Option<Intent> {
        let title = self.title();
        let subtitle = format!("{} / {} / {}", self.table.db, self.table.schema, self.table.table);
        let mut intent = None;
        crate::ui::workflow_dialog::show(ctx, egui::Id::new("table_export"), title, &subtitle, vec2(680.0, 540.0), |root, regions| {
            let mut body = regions.body_ui(root);
            egui::ScrollArea::both().auto_shrink([false, false]).show(&mut body, |ui| {
                ui.add_enabled_ui(!self.pending, |ui| {
                    match &mut self.kind {
                        ExportKind::Print { data } => {
                            ui.add_enabled(table_export::supported(self.driver), egui::Checkbox::new(data, tr("Include all table data")));
                            ui.label(tr("The report opens in your browser. Use Print to choose a printer or PDF."));
                        }
                        ExportKind::Sql { structure, data } => {
                            ui.label(tr(if *structure && *data {
                                "Structure and Data"
                            } else if *structure {
                                "Structure Only"
                            } else {
                                "Data Only"
                            }));
                        }
                        kind => {
                            egui::ComboBox::from_id_salt("table_export_format").selected_text(format_name(*kind)).show_ui(ui, |ui| {
                                for format in [ExportKind::Csv, ExportKind::Tsv, ExportKind::Json] {
                                    ui.selectable_value(kind, format, format_name(format));
                                }
                            });
                        }
                    }
                    ui.add_space(10.0);
                    ui.label(tr("Destination file"));
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.path).desired_width((ui.available_width() - 90.0).max(40.0)));
                        if ui.button(tr("Browse…")).clicked()
                            && let Some(path) = rfd::FileDialog::new()
                                .add_filter(format_name(self.kind), &[extension(self.kind)])
                                .set_file_name(format!("{}.{}", self.table.table, extension(self.kind)))
                                .save_file()
                        {
                            self.path = path.to_string_lossy().into_owned();
                        }
                    });
                    ui.checkbox(&mut self.overwrite, tr("Replace file?"))
                        .on_hover_text(tr("The destination already exists. Replace it only after the export succeeds?"));
                    ui.label(tr("Exports read the complete table, independently of the data editor's page and filters."));
                });
                if let Some(error) = &self.error {
                    ui.add(egui::Label::new(RichText::new(error).color(palette(ctx.global_style().visuals.dark_mode).error)).wrap());
                }
                if let Some(success) = &self.success {
                    ui.add(egui::Label::new(success).wrap());
                }
            });
            let mut footer = regions.footer_ui(root);
            footer.horizontal(|ui| {
                if self.pending {
                    ui.spinner();
                    ui.label(trf("{n} rows processed", &[("n", &self.progress.rows.load(Ordering::Relaxed))]));
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(tr(if self.pending { "Cancel" } else { "Close" })).clicked() {
                        if self.pending {
                            self.progress.cancel.store(true, Ordering::Relaxed);
                        } else {
                            intent = Some(Intent::Close);
                        }
                    }
                    if ui
                        .add_enabled(
                            !self.pending && !self.path.trim().is_empty(),
                            egui::Button::new(tr(if matches!(self.kind, ExportKind::Print { .. }) { "Create report" } else { "Export" })),
                        )
                        .clicked()
                    {
                        intent = Some(Intent::Export);
                    }
                });
            });
        });
        if !self.pending && ctx.input(|i| i.key_pressed(Key::Escape)) {
            intent = Some(Intent::Close);
        }
        intent
    }
}

fn extension(kind: ExportKind) -> &'static str {
    match kind {
        ExportKind::Csv => "csv",
        ExportKind::Tsv => "tsv",
        ExportKind::Json => "json",
        ExportKind::Sql { .. } => "sql",
        ExportKind::Print { .. } => "html",
    }
}
fn format_name(kind: ExportKind) -> &'static str {
    match kind {
        ExportKind::Csv => "CSV",
        ExportKind::Tsv => "TSV",
        ExportKind::Json => "JSON",
        ExportKind::Sql { .. } => "SQL",
        ExportKind::Print { .. } => "HTML",
    }
}

impl DataCraftApp {
    pub(super) fn open_table_export(&mut self, table: TableRef, kind: ExportKind) {
        if self.table_export.dialog.as_ref().is_some_and(|d| d.pending) {
            self.error(tr("Wait for the current table operation to finish"));
            return;
        }
        let Some(connection) = self.conn(table.conn) else { return };
        if connection.status != ConnStatus::Connected {
            self.error(tr("The connection is not open"));
            return;
        }
        let driver = connection.cfg.driver;
        if !table_export::supported_kind(driver, kind) {
            self.error(tr("Complete table export is not available for this engine"));
            return;
        }
        if !self.table_info(table.conn, Some(&table.db), &table.schema, &table.table).is_some_and(|t| t.kind == crate::db::ObjectKind::Table) {
            self.error(tr("Select one supported table"));
            return;
        }
        self.table_export.dialog = Some(Dialog {
            id: 0,
            table,
            driver,
            kind,
            path: String::new(),
            overwrite: false,
            pending: false,
            progress: Arc::default(),
            error: None,
            success: None,
        });
    }

    fn start_table_export(&mut self) {
        let Some(d) = self.table_export.dialog.as_mut().filter(|d| !d.pending && !d.path.trim().is_empty()) else { return };
        let path = PathBuf::from(d.path.trim());
        if matches!(d.kind, ExportKind::Print { .. }) && !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("html") || e.eq_ignore_ascii_case("htm")) {
            d.error = Some(tr("Choose an HTML file for the print report").into());
            return;
        }
        d.id = uuid::Uuid::new_v4().as_u128() as u64;
        d.progress = Arc::default();
        d.pending = true;
        d.error = None;
        d.success = None;
        self.worker.send(Request::ExportTable {
            conn: d.table.conn,
            db: d.table.db.clone(),
            request_id: d.id,
            request: Box::new(ExportRequest { schema: d.table.schema.clone(), table: d.table.table.clone(), path, kind: d.kind, overwrite: d.overwrite }),
            progress: d.progress.clone(),
        });
    }

    pub(super) fn show_table_export(&mut self, ctx: &egui::Context) {
        match self.table_export.dialog.as_mut().and_then(|d| d.show(ctx)) {
            Some(Intent::Export) => self.start_table_export(),
            Some(Intent::Close) => self.table_export.dialog = None,
            None => {}
        }
    }

    /// Returns a report path only for the current, explicitly submitted Print operation.
    pub(super) fn table_export_received(&mut self, conn: ConnId, request_id: u64, result: Result<(PathBuf, u64), String>) -> Option<PathBuf> {
        let d = self.table_export.dialog.as_mut().filter(|d| d.table.conn == conn && d.id == request_id && d.pending)?;
        d.pending = false;
        match result {
            Err(error) => {
                d.error = Some(error);
                None
            }
            Ok((path, rows)) => {
                d.success = Some(trf("Exported {n} rows to {path}", &[("n", &rows), ("path", &path.to_string_lossy())]));
                (matches!(d.kind, ExportKind::Print { .. }) && !d.progress.cancel.load(Ordering::Relaxed)).then_some(path)
            }
        }
    }

    pub(super) fn table_report_opened(&mut self, request_id: u64, result: Result<PathBuf, String>) {
        if let Some(d) = self.table_export.dialog.as_mut().filter(|d| d.id == request_id)
            && let Err(error) = result
        {
            d.error = Some(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::NodeT;

    fn app_and_table() -> (DataCraftApp, TableRef) {
        let mut app = super::super::tests::tabs_app();
        let mut connection = ConnState::new(ConnectionConfig::new(7, Driver::Sqlite));
        connection.status = ConnStatus::Connected;
        connection.databases = vec![DatabaseInfo {
            name: "main".into(),
            current: true,
            schemas: Some(vec![SchemaInfo {
                name: "main".into(),
                tables: vec![TableInfo { name: "notes".into(), kind: crate::db::ObjectKind::Table, columns: vec![] }],
            }]),
        }];
        app.conns.push(connection);
        (app, TableRef { conn: 7, db: "main".into(), schema: "main".into(), table: "notes".into() })
    }

    #[test]
    fn export_actions_open_read_only_workflows_and_reject_disconnected_connections() {
        let (mut app, table) = app_and_table();
        app.conns[0].cfg.read_only = true;
        for (action, title) in [
            (ExplorerAction::ExportTable(table.clone()), "Export Table"),
            (ExplorerAction::DumpTable(table.clone(), false, true), "Dump SQL File"),
            (ExplorerAction::PrintTable(table.clone()), "Print Table"),
        ] {
            app.handle_explorer(&egui::Context::default(), vec![action]);
            assert_eq!(app.table_export.dialog.as_ref().unwrap().title(), tr(title));
        }
        assert!(matches!(app.table_export.dialog.as_ref().unwrap().kind, ExportKind::Print { data: false }));
        app.table_export.dialog = None;
        app.conns[0].status = ConnStatus::Disconnected;
        app.open_table_export(table, ExportKind::Csv);
        assert!(app.table_export.dialog.is_none());
    }

    #[test]
    fn stale_or_non_print_results_never_request_browser_open() {
        let (mut app, table) = app_and_table();
        for kind in [ExportKind::Csv, ExportKind::Print { data: false }] {
            app.open_table_export(table.clone(), kind);
            let d = app.table_export.dialog.as_mut().unwrap();
            d.id = 12;
            d.pending = true;
            assert!(app.table_export_received(7, 11, Ok(("old.html".into(), 9))).is_none());
            assert!(app.table_export.dialog.as_ref().unwrap().pending);
            let path = app.table_export_received(7, 12, Ok(("report.html".into(), 1200)));
            assert_eq!(path.is_some(), matches!(kind, ExportKind::Print { .. }));
        }
    }

    #[test]
    fn invalid_print_destination_is_recoverable_before_worker_submission() {
        let (mut app, table) = app_and_table();
        app.open_table_export(table, ExportKind::Print { data: false });
        app.table_export.dialog.as_mut().unwrap().path = "report.exe".into();
        app.start_table_export();
        let d = app.table_export.dialog.as_ref().unwrap();
        assert!(!d.pending);
        assert!(d.error.is_some());
    }

    #[test]
    fn export_modals_block_console_run_and_save_shortcuts_until_closed() {
        fn press(app: &mut DataCraftApp, key: Key, modifiers: Modifiers) {
            let ctx = egui::Context::default();
            let mut output = ctx.run_ui(
                egui::RawInput { events: vec![egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }], ..Default::default() },
                |ui| app.shortcuts(ui.ctx()),
            );
            // This headless input test has no renderer to apply texture deltas.
            output.textures_delta.clear();
        }
        for kind in [ExportKind::Csv, ExportKind::Sql { structure: true, data: true }, ExportKind::Print { data: false }] {
            for pending in [false, true] {
                for (key, modifiers) in [(Key::Enter, Modifiers::COMMAND), (Key::Enter, Modifiers::COMMAND | Modifiers::SHIFT), (Key::S, Modifiers::COMMAND)] {
                    let (mut app, table) = app_and_table();
                    let active = app.active;
                    app.object_browser.active = false;
                    app.consoles[active].conn = Some(table.conn);
                    app.consoles[active].db = Some(table.db.clone());
                    app.consoles[active].text = "SELECT 42".into();
                    if key == Key::S {
                        let mut view = ResultView::new(
                            crate::db::ResultSet {
                                columns: vec!["id".into(), "note".into()],
                                rows: vec![vec![Some("1".into()), Some("before".into())]],
                                ..Default::default()
                            },
                            901,
                        );
                        view.set_edit_target(Ok(editing::EditTarget {
                            schema: "main".into(),
                            table: "notes".into(),
                            col_names: vec!["id".into(), "note".into()],
                            key_cols: vec![0],
                            binary_cols: vec![false, false],
                            has_pk: true,
                            insert_only: false,
                        }));
                        view.edit.as_mut().unwrap().state.set_value(&view.rs, RowRef::Existing(0), 1, Some("after".into()));
                        app.consoles[active].results.push(view);
                        app.consoles[active].result_tab = ResultTab::Result(0);
                    }
                    app.open_table_export(table, kind);
                    app.table_export.dialog.as_mut().unwrap().pending = pending;
                    press(&mut app, key, modifiers);
                    assert!(!app.consoles[active].running, "{kind:?}, pending={pending}, key={key:?}");
                    assert!(app.preview.is_none());
                    assert!(app.modal_open(), "native-menu commands must use the same gate");
                    app.table_export.dialog = None;
                    press(&mut app, key, modifiers);
                    assert!(app.consoles[active].running, "the same shortcut must work after closing the dialog: {key:?}");
                    if key == Key::S {
                        assert!(app.preview.as_ref().unwrap().applying);
                    }
                }
            }
        }
    }

    #[test]
    fn export_menu_allows_read_only_tables_and_disables_ambiguous_selection() {
        use egui_kittest::kittest::Queryable;
        let (_, table) = app_and_table();
        for count in [1, 2] {
            let mut tables = vec![table.clone()];
            if count == 2 {
                let mut other = table.clone();
                other.table = "other".into();
                tables.push(other);
            }
            let mut h = egui_kittest::Harness::builder().with_size(vec2(800.0, 700.0)).build_ui_state(
                move |ui, actions: &mut Vec<ExplorerAction>| {
                    crate::ui::table_menu::show(
                        ui,
                        &tables,
                        Some(crate::db::ObjectKind::Table),
                        crate::ui::table_menu::TableContext { driver: Driver::Sqlite, connected: true, read_only: true },
                        actions,
                    );
                },
                Vec::new(),
            );
            h.run_steps(2);
            assert_eq!(h.get_by_label("Export Table…").accesskit_node().is_disabled(), count == 2);
            assert_eq!(h.get_by_label("Print Table…").accesskit_node().is_disabled(), count == 2);
            if count == 1 {
                h.get_by_label("Export Table…").click();
                h.run_steps(2);
                assert!(matches!(h.state().first(), Some(ExplorerAction::ExportTable(t)) if t.table == "notes"));
            }
        }
    }

    #[test]
    fn snowflake_dump_menu_offers_structure_without_enabling_data_export() {
        use egui_kittest::kittest::Queryable;
        let (_, table) = app_and_table();
        let mut h = egui_kittest::Harness::builder().with_size(vec2(800.0, 700.0)).build_ui_state(
            move |ui, actions: &mut Vec<ExplorerAction>| {
                crate::ui::table_menu::show(
                    ui,
                    std::slice::from_ref(&table),
                    Some(crate::db::ObjectKind::Table),
                    crate::ui::table_menu::TableContext { driver: Driver::Snowflake, connected: true, read_only: true },
                    actions,
                );
            },
            Vec::new(),
        );
        h.run_steps(2);
        assert!(h.get_by_label("Export Table…").accesskit_node().is_disabled());
        h.get_by_label("Dump SQL File").click();
        h.run_steps(2);
        assert!(h.get_by_label("Structure and Data").accesskit_node().is_disabled());
        assert!(h.get_by_label("Data Only").accesskit_node().is_disabled());
        h.get_by_label("Structure Only").click();
        h.run_steps(2);
        assert!(matches!(h.state().first(), Some(ExplorerAction::DumpTable(_, true, false))));
    }

    #[test]
    fn export_footer_remains_visible_in_small_windows_and_cancel_keeps_dialog_until_reply() {
        use egui_kittest::kittest::Queryable;
        let (mut app, table) = app_and_table();
        app.open_table_export(table, ExportKind::Csv);
        let d = app.table_export.dialog.as_mut().unwrap();
        d.pending = true;
        d.error = Some("Long error\n".repeat(80));
        let mut first = true;
        let mut h = egui_kittest::Harness::builder().with_size(vec2(600.0, 370.0)).build_ui_state(
            move |ui, app: &mut DataCraftApp| {
                if std::mem::take(&mut first) {
                    theme::install_fonts(ui.ctx());
                    return;
                }
                app.show_table_export(ui.ctx());
            },
            app,
        );
        h.run_steps(3);
        assert!(h.get_by_label("Cancel").rect().bottom() < 370.0);
        h.get_by_label("Cancel").click();
        h.run_steps(2);
        let d = h.state().table_export.dialog.as_ref().unwrap();
        assert!(d.pending);
        assert!(d.progress.cancel.load(Ordering::Relaxed));
    }
}
