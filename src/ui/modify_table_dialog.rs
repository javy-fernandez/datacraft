//! "Modify Table…": edits an existing table's name, columns, primary key and indexes, and runs
//! the ALTER TABLE statements of the difference, shown first in a preview.

use crate::db::Driver;
use crate::db::alter::{self, EditColumn, EditIndex, Plan, TableDetails, TableEdit};
use crate::i18n::{tr, trf};
use crate::ui::create_dialog::{columns_editor, sql_preview};
use crate::ui::icons::Ico;
use crate::ui::theme::{self, palette};
use crate::worker::ConnId;
use egui::{FontId, Margin, RichText, Ui, vec2};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);

pub struct ModifyTableDialog {
    pub conn: ConnId,
    pub driver: Driver,
    pub db: String,
    pub schema: String,
    pub table: String,
    /// Id of the `TableDetails` request whose reply fills the dialog.
    pub request_id: u64,
    /// Structure as loaded (None while loading).
    details: Option<TableDetails>,
    load_error: Option<String>,
    pub new_name: String,
    pub columns: Vec<EditColumn>,
    pub indexes: Vec<EditIndex>,
    /// Statements sent to the worker; waiting for the reply.
    pub running: bool,
    pub error: Option<String>,
}

pub enum ModifyResult {
    Apply(Plan),
    Cancel,
}

impl ModifyTableDialog {
    pub fn new(conn: ConnId, driver: Driver, db: String, schema: String, table: String) -> Self {
        Self {
            conn,
            driver,
            db,
            schema,
            new_name: table.clone(),
            table,
            request_id: NEXT_REQUEST.fetch_add(1, Ordering::Relaxed),
            details: None,
            load_error: None,
            columns: vec![],
            indexes: vec![],
            running: false,
            error: None,
        }
    }

    /// Reply to the structure request.
    pub fn loaded(&mut self, result: Result<TableDetails, String>) {
        match result {
            Ok(d) => {
                self.columns = alter::initial_columns(&d);
                self.indexes = alter::initial_indexes(&d);
                self.details = Some(d);
            }
            Err(e) => self.load_error = Some(e),
        }
    }

    pub fn is_loaded(&self) -> bool {
        self.details.is_some()
    }

    /// Name the table has after the change (Oracle/Snowflake: simple names in uppercase).
    pub fn final_name(&self) -> String {
        if self.new_name.trim() == self.table {
            self.table.clone()
        } else {
            crate::ui::drop_dialog::effective_name(self.driver, &self.new_name)
        }
    }

    pub fn plan(&self) -> Option<Result<Plan, String>> {
        let d = self.details.as_ref()?;
        let edit = TableEdit {
            schema: &self.schema,
            table: &self.table,
            new_name: &self.new_name,
            columns: &self.columns,
            indexes: &self.indexes,
        };
        Some(alter::plan(self.driver, d, &edit))
    }

    /// Demo mode: a few sample edits so the preview has something to show.
    pub fn demo_edits(&mut self) {
        if let Some(c) = self.columns.iter_mut().find(|c| !c.column.primary_key) {
            c.column.name.push_str("_new");
        }
        self.columns.push(EditColumn {
            original: None,
            column: crate::ui::create_dialog::NewColumn {
                name: "created_at".into(),
                data_type: "TEXT".into(),
                not_null: false,
                primary_key: false,
                default: "CURRENT_TIMESTAMP".into(),
            },
        });
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Option<ModifyResult> {
        let mut result = None;
        let p = palette(ctx.global_style().visuals.dark_mode);
        let frame = egui::Frame::popup(&ctx.global_style())
            .fill(p.popup_bg)
            .corner_radius(12)
            .inner_margin(Margin::same(20))
            .stroke(egui::Stroke::new(1.0, p.popup_border));
        let plan = self.plan();
        egui::Modal::new(egui::Id::new("modify_table_dialog"))
            .frame(frame)
            .show(ctx, |ui| {
                ui.set_width(680.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(tr("Modify Table"))
                            .font(theme::bold(15.0))
                            .color(p.text),
                    );
                    ui.add_space(6.0);
                    ui.label(RichText::new(trf("in {db}", &[("db", &self.db)])).color(p.muted));
                });
                ui.add_space(14.0);
                if self.details.is_none() {
                    match &self.load_error {
                        Some(e) => {
                            ui.add(
                                egui::Label::new(RichText::new(e).monospace().color(p.error))
                                    .wrap(),
                            );
                        }
                        None => {
                            ui.horizontal(|ui| {
                                ui.add(egui::Spinner::new().size(14.0).color(p.muted));
                                ui.label(
                                    RichText::new(tr("Loading table structure…")).color(p.muted),
                                );
                            });
                        }
                    }
                } else {
                    ui.add_enabled_ui(!self.running, |ui| {
                        egui::Grid::new("modify_form")
                            .num_columns(2)
                            .spacing([14.0, 10.0])
                            .min_col_width(80.0)
                            .show(ui, |ui| {
                                ui.label(tr("Name:"));
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.new_name)
                                        .margin(vec2(8.0, 5.0))
                                        .desired_width(f32::INFINITY),
                                );
                                ui.end_row();
                            });
                        ui.add_space(12.0);
                        columns_editor(
                            ui,
                            "modify",
                            self.driver,
                            &mut self.columns,
                            |e| &mut e.column,
                            |c| EditColumn {
                                original: None,
                                column: c,
                            },
                            1,
                        );
                        if alter::has_indexes(self.driver) {
                            ui.add_space(12.0);
                            self.indexes_editor(ui);
                        }
                    });
                    ui.add_space(12.0);
                    match &plan {
                        Some(Ok(plan)) => {
                            for w in &plan.warnings {
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new("⚠").color(p.warn));
                                    ui.add(egui::Label::new(RichText::new(w).color(p.warn)).wrap());
                                });
                            }
                            if !plan.warnings.is_empty() {
                                ui.add_space(6.0);
                            }
                            let text = if plan.statements.is_empty() {
                                format!("-- {}", tr("No changes"))
                            } else {
                                script(plan)
                            };
                            sql_preview(ui, "modify", &text);
                        }
                        Some(Err(e)) => {
                            ui.add(egui::Label::new(RichText::new(e).color(p.error)).wrap());
                        }
                        None => {}
                    }
                }
                if let Some(e) = &self.error {
                    ui.add_space(8.0);
                    ui.add(egui::Label::new(RichText::new(e).monospace().color(p.error)).wrap());
                }
                ui.add_space(14.0);
                let ready = plan
                    .as_ref()
                    .and_then(|p| p.as_ref().ok())
                    .filter(|p| !p.statements.is_empty());
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled_ui(ready.is_some(), |ui| {
                            theme::secondary_button(ui, tr("Copy SQL"))
                        })
                        .inner
                        .clicked()
                        && let Some(plan) = ready
                    {
                        ui.ctx().copy_text(script(plan));
                    }
                    if self.running {
                        ui.add_space(8.0);
                        ui.add(egui::Spinner::new().size(14.0).color(p.muted));
                        ui.label(RichText::new(tr("Applying changes…")).color(p.muted));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        ui.add_enabled_ui(!self.running && ready.is_some(), |ui| {
                            if theme::primary_button(ui, tr("Apply")).clicked()
                                && let Some(Ok(plan)) = self.plan()
                            {
                                result = Some(ModifyResult::Apply(plan));
                            }
                        });
                        ui.add_enabled_ui(!self.running, |ui| {
                            if theme::secondary_button(ui, tr("Cancel")).clicked() {
                                result = Some(ModifyResult::Cancel);
                            }
                        });
                    });
                });
            });
        if result.is_none() && !self.running && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            result = Some(ModifyResult::Cancel);
        }
        result
    }

    fn indexes_editor(&mut self, ui: &mut Ui) {
        let p = theme::pal(ui);
        ui.label(RichText::new(tr("Indexes")).font(theme::bold(13.0)));
        ui.add_space(6.0);
        let mut remove = None;
        if !self.indexes.is_empty() {
            egui::ScrollArea::vertical()
                .id_salt("modify_indexes")
                .max_height(140.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    egui::Grid::new("modify_indexes_grid")
                        .num_columns(4)
                        .spacing([8.0, 6.0])
                        .show(ui, |ui| {
                            for h in [tr("Name"), tr("Columns"), tr("Unique"), ""] {
                                ui.label(RichText::new(h).small().color(p.muted));
                            }
                            ui.end_row();
                            for (i, x) in self.indexes.iter_mut().enumerate() {
                                // Existing indexes can only be dropped.
                                let new = x.original.is_none();
                                // add_sized: a grid cell would otherwise shrink the fields to their text.
                                ui.add_enabled_ui(new, |ui| {
                                    ui.add_sized(
                                        [200.0, 24.0],
                                        egui::TextEdit::singleline(&mut x.name)
                                            .id_salt(("index_name", i))
                                            .margin(vec2(6.0, 4.0)),
                                    );
                                });
                                ui.add_enabled_ui(new, |ui| {
                                    ui.add_sized(
                                        [280.0, 24.0],
                                        egui::TextEdit::singleline(&mut x.columns)
                                            .id_salt(("index_columns", i))
                                            .hint_text(tr("column1, column2"))
                                            .font(egui::TextStyle::Monospace)
                                            .margin(vec2(6.0, 4.0)),
                                    );
                                });
                                ui.add_enabled(new, egui::Checkbox::without_text(&mut x.unique));
                                if theme::icon_button(ui, Ico::Minus, true, tr("Remove index"))
                                    .clicked()
                                {
                                    remove = Some(i);
                                }
                                ui.end_row();
                            }
                        });
                });
        }
        if let Some(i) = remove {
            self.indexes.remove(i);
        }
        ui.add_space(4.0);
        if theme::flat_button(
            ui,
            Some(Ico::Plus),
            tr("Add Index"),
            FontId::proportional(13.0),
            false,
            true,
        )
        .clicked()
        {
            let table = self.new_name.trim().to_string();
            let n = self.indexes.len() + 1;
            self.indexes.push(EditIndex {
                original: None,
                name: format!("{table}_idx{n}"),
                columns: String::new(),
                unique: false,
            });
        }
    }
}

/// The statements as a script, for the preview and "Copy SQL".
pub fn script(plan: &Plan) -> String {
    plan.statements
        .iter()
        .map(|s| format!("{s};"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes previews of "Modify Table…" and "Edit View…" to target/dialog-snapshots, on a real
    /// SQLite table and view.
    #[test]
    #[ignore = "Requires a GPU; writes dialog previews to target/dialog-snapshots"]
    fn render_modify_table_and_view() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/dialog-snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        let db = std::env::temp_dir().join(format!("datacraft-render-{}.db", uuid::Uuid::new_v4()));
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch(
                "create table customers(id integer primary key, name varchar(80) not null, email text, created_at datetime default current_timestamp);
                 create index ix_email on customers(email);
                 create view active_customers as select id, name from customers where email is not null;",
            )
            .unwrap();
        let mut cfg = crate::db::ConnectionConfig::new(1, Driver::Sqlite);
        cfg.file_path = db.display().to_string();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (details, view) = rt.block_on(async {
            let mut s = crate::db::Session::connect(&cfg).await.unwrap();
            let details = alter::load_details(&mut s, Driver::Sqlite, "main", "customers")
                .await
                .unwrap();
            let view = s
                .view_definition(Driver::Sqlite, "main", "main", "active_customers")
                .await
                .unwrap();
            (details, view)
        });
        let _ = std::fs::remove_file(&db);

        let mut d = ModifyTableDialog::new(
            1,
            Driver::Sqlite,
            "main".into(),
            "main".into(),
            "customers".into(),
        );
        d.loaded(Ok(details));
        d.demo_edits();
        let mut first = true;
        let mut h = egui_kittest::Harness::builder()
            .with_size(vec2(900.0, 860.0))
            .build_ui_state(
                move |ui, d: &mut ModifyTableDialog| {
                    if std::mem::take(&mut first) {
                        theme::install_fonts(ui.ctx());
                        theme::apply(ui.ctx(), false);
                        return;
                    }
                    d.show(ui.ctx());
                },
                d,
            );
        h.run_steps(4);
        h.render()
            .unwrap()
            .save(dir.join("modify-table.png"))
            .unwrap();

        let mut v = crate::ui::view_dialog::ViewDialog::edit_view(
            1,
            Driver::Sqlite,
            "main".into(),
            "main".into(),
            "active_customers".into(),
            1,
        );
        v.definition_loaded(Ok(view));
        let mut first = true;
        let mut h = egui_kittest::Harness::builder()
            .with_size(vec2(900.0, 760.0))
            .build_ui_state(
                move |ui, v: &mut crate::ui::view_dialog::ViewDialog| {
                    if std::mem::take(&mut first) {
                        theme::install_fonts(ui.ctx());
                        theme::apply(ui.ctx(), false);
                        return;
                    }
                    v.show(ui.ctx(), &[]);
                },
                v,
            );
        h.run_steps(4);
        h.render().unwrap().save(dir.join("edit-view.png")).unwrap();
    }
}
