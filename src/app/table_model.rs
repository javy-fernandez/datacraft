//! Opening, cancelling and saving a local model of declared table relationships.

use super::*;
use crate::db::table_model::Model;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(super) struct Dialog {
    id: u64,
    table: TableRef,
    cancel: Arc<AtomicBool>,
    view: Option<crate::ui::table_model::ModelView>,
    error: Option<String>,
}

enum Action {
    Close,
    Save(bool),
}

impl Dialog {
    fn show(&mut self, ctx: &egui::Context) -> Option<Action> {
        let mut action = None;
        let pending = self.view.is_none() && self.error.is_none();
        crate::ui::workflow_dialog::show(
            ctx,
            egui::Id::new("table_model"),
            tr("Table Model"),
            &format!("{} / {} / {}", self.table.db, self.table.schema, self.table.table),
            vec2(1000.0, 700.0),
            |root, regions| {
                let mut body = regions.body_ui(root);
                egui::ScrollArea::both().auto_shrink([false, false]).show(&mut body, |ui| {
                    if let Some(view) = &mut self.view {
                        ui.horizontal(|ui| {
                            if ui.button(tr("Save model…")).clicked() {
                                action = Some(Action::Save(false));
                            }
                            if ui.button(tr("Export SVG…")).clicked() {
                                action = Some(Action::Save(true));
                            }
                        });
                        view.show(ui);
                    }
                    if let Some(error) = &self.error {
                        ui.add(egui::Label::new(RichText::new(error).color(palette(ctx.global_style().visuals.dark_mode).error)).wrap());
                    }
                    if pending {
                        ui.spinner();
                        ui.label(tr("Loading…"));
                    }
                });
                regions.footer_ui(root).with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(tr(if pending { "Cancel" } else { "Close" })).clicked() {
                        action = Some(Action::Close);
                    }
                });
            },
        );
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            action = Some(Action::Close);
        }
        action
    }
}

impl DataCraftApp {
    pub(super) fn open_table_model(&mut self, tables: Vec<TableRef>) {
        let Some(table) = tables.first().cloned() else {
            return;
        };
        let Some(connection) = self.conn(table.conn).filter(|c| c.status == ConnStatus::Connected) else {
            self.error(tr("The connection is not open"));
            return;
        };
        if !crate::db::table_model::supported(connection.cfg.driver) {
            self.error(tr("Not available for this engine"));
            return;
        }
        if tables.iter().any(|t| t.conn != table.conn || t.db != table.db) {
            self.error(tr("Select tables in one database"));
            return;
        }
        if let Some(previous) = self.table_model.take() {
            previous.cancel.store(true, Ordering::Relaxed);
        }
        let id = uuid::Uuid::new_v4().as_u128() as u64;
        let cancel = Arc::new(AtomicBool::new(false));
        self.worker.send(Request::TableModel {
            conn: table.conn,
            db: table.db.clone(),
            request_id: id,
            tables: tables.into_iter().map(|t| (t.schema, t.table)).collect(),
            cancel: Arc::clone(&cancel),
        });
        self.table_model = Some(Dialog { id, table, cancel, view: None, error: None });
    }

    pub(super) fn table_model_received(&mut self, request_id: u64, result: Result<Model, String>) {
        let Some(dialog) = self.table_model.as_mut().filter(|d| d.id == request_id) else {
            return;
        };
        match result {
            Ok(model) => dialog.view = Some(crate::ui::table_model::ModelView::new(model)),
            Err(error) => dialog.error = Some(error),
        }
    }

    pub(super) fn show_table_model(&mut self, ctx: &egui::Context) {
        match self.table_model.as_mut().and_then(|d| d.show(ctx)) {
            Some(Action::Close) => {
                if let Some(dialog) = self.table_model.take() {
                    dialog.cancel.store(true, Ordering::Relaxed);
                }
            }
            Some(Action::Save(svg)) => {
                let Some(view) = self.table_model.as_ref().and_then(|d| d.view.as_ref()) else {
                    return;
                };
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter(if svg { "SVG" } else { "JSON" }, &[if svg { "svg" } else { "json" }])
                    .set_file_name(if svg { "table-model.svg" } else { "table-model.json" })
                    .save_file()
                {
                    let contents = if svg { Ok(view.svg()) } else { serde_json::to_string_pretty(&view.model) };
                    match contents {
                        Ok(contents) => self.worker.send(Request::SaveModel { path, contents }),
                        Err(error) => self.error(error.to_string()),
                    }
                }
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_dialog_blocks_running_the_hidden_sql_console() {
        let (mut app, table) = super::super::tests::table_menu_app(Driver::Sqlite, true);
        let active = app.active;
        app.consoles[active].conn = Some(table.conn);
        app.consoles[active].db = Some(table.db.clone());
        app.consoles[active].text = "SELECT 1".into();
        app.object_browser.active = false;
        app.open_table_model(vec![table]);
        let mut h = egui_kittest::Harness::builder().build_ui_state(|ui, app: &mut DataCraftApp| app.shortcuts(ui.ctx()), app);
        h.key_press_modifiers(Modifiers::COMMAND, Key::Enter);
        h.run_steps(2);
        assert!(!h.state().consoles[active].running, "the model dialog must block the hidden console");
        h.state_mut().table_model = None;
        h.key_press_modifiers(Modifiers::COMMAND, Key::Enter);
        h.run_steps(2);
        assert!(h.state().consoles[active].running, "the same shortcut works after closing the dialog");
    }

    #[test]
    fn stale_model_responses_cannot_replace_a_new_selection() {
        let (mut app, table) = super::super::tests::table_menu_app(Driver::Sqlite, true);
        app.conns[0].cfg.read_only = true;
        app.open_table_model(vec![table.clone()]);
        let old = app.table_model.as_ref().unwrap().id;
        let old_cancel = Arc::clone(&app.table_model.as_ref().unwrap().cancel);
        app.open_table_model(vec![table]);
        assert!(old_cancel.load(Ordering::Relaxed));
        app.table_model_received(old, Ok(Model { tables: vec![], relations: vec![] }));
        assert!(app.table_model.as_ref().unwrap().view.is_none());
        let current = app.table_model.as_ref().unwrap().id;
        app.table_model_received(current, Ok(Model { tables: vec![], relations: vec![] }));
        assert!(app.table_model.as_ref().unwrap().view.is_some());
    }
}
