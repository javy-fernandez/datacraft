//! "New View" / "Edit View" modal dialog: name, schema, the SELECT in a highlighted editor, the
//! engine's options and a live preview of the statements it runs.

use crate::db::Driver;
use crate::db::views::{self, ViewDefinition, ViewSpec, ViewStatements};
use crate::i18n::{tr, trf};
use crate::sql;
use crate::ui::theme::{self, palette};
use crate::worker::ConnId;
use egui::{FontId, Margin, RichText, Ui, vec2};

pub struct ViewDialog {
    pub conn: ConnId,
    pub driver: Driver,
    pub db: String,
    pub spec: ViewSpec,
    /// Editing: the definition request in flight (`None` once it arrived).
    pub loading: Option<u64>,
    /// Statements sent to the worker; waiting for the reply.
    pub running: bool,
    pub error: Option<String>,
    focus_name: bool,
}

pub enum ViewResult {
    Run(ViewStatements),
    Cancel,
}

const ALGORITHMS: [&str; 4] = ["", "UNDEFINED", "MERGE", "TEMPTABLE"];
const SECURITIES: [&str; 3] = ["", "DEFINER", "INVOKER"];

impl ViewDialog {
    pub fn new_view(conn: ConnId, driver: Driver, db: String, schema: String) -> Self {
        Self {
            conn,
            driver,
            db,
            spec: ViewSpec {
                schema,
                body: "select\nfrom ".into(),
                ..Default::default()
            },
            loading: None,
            running: false,
            error: None,
            focus_name: true,
        }
    }

    /// Edits `schema.name`; its definition arrives later (`request_id`).
    pub fn edit_view(
        conn: ConnId,
        driver: Driver,
        db: String,
        schema: String,
        name: String,
        request_id: u64,
    ) -> Self {
        Self {
            conn,
            driver,
            db,
            spec: ViewSpec {
                schema,
                original: Some(name.clone()),
                name,
                ..Default::default()
            },
            loading: Some(request_id),
            running: false,
            error: None,
            focus_name: false,
        }
    }

    pub fn is_edit(&self) -> bool {
        self.spec.original.is_some()
    }

    /// The definition read from the server (or why it couldn't be).
    pub fn definition_loaded(&mut self, result: Result<ViewDefinition, String>) {
        self.loading = None;
        match result {
            Ok(def) => {
                self.spec.body = def.body;
                self.spec.columns = def.columns;
                self.spec.algorithm = def.algorithm;
                self.spec.security = def.security;
            }
            Err(e) => self.error = Some(e),
        }
    }

    pub fn statements(&self) -> ViewStatements {
        views::view_statements(self.driver, &self.spec)
    }

    fn preview_text(&self) -> String {
        self.statements()
            .statements
            .iter()
            .map(|s| format!("{s};"))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    pub fn validate(&self) -> Result<ViewStatements, String> {
        if self.spec.name.trim().is_empty() {
            return Err(tr("Name is required").into());
        }
        if views::clean_body(&self.spec.body).is_empty() {
            return Err(tr("The SELECT query is required").into());
        }
        Ok(self.statements())
    }

    /// `schemas`: the database's schemas, to choose from (empty if not loaded yet).
    pub fn show(&mut self, ctx: &egui::Context, schemas: &[String]) -> Option<ViewResult> {
        let mut result = None;
        let p = palette(ctx.global_style().visuals.dark_mode);
        let frame = egui::Frame::popup(&ctx.global_style())
            .fill(p.popup_bg)
            .corner_radius(12)
            .inner_margin(Margin::same(20))
            .stroke(egui::Stroke::new(1.0, p.popup_border));
        let edit = self.is_edit();
        egui::Modal::new(egui::Id::new("view_dialog"))
            .frame(frame)
            .show(ctx, |ui| {
                ui.set_width(680.0);
                ui.horizontal(|ui| {
                    let title = if edit {
                        tr("Edit View")
                    } else {
                        tr("New View")
                    };
                    ui.label(RichText::new(title).font(theme::bold(15.0)).color(p.text));
                    ui.add_space(6.0);
                    ui.label(RichText::new(trf("in {db}", &[("db", &self.db)])).color(p.muted));
                });
                ui.add_space(14.0);
                let busy = self.running || self.loading.is_some();
                ui.add_enabled_ui(!busy, |ui| {
                    self.form(ui, schemas);
                    ui.add_space(10.0);
                    ui.label(RichText::new(tr("Query:")).color(p.text));
                    ui.add_space(4.0);
                    self.editor(ui);
                });
                if self.loading.is_some() {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(14.0).color(p.muted));
                        ui.label(RichText::new(tr("Loading the view definition…")).color(p.muted));
                    });
                }
                ui.add_space(10.0);
                self.preview(ui);
                if let Some(e) = &self.error {
                    ui.add_space(8.0);
                    ui.add(egui::Label::new(RichText::new(e).monospace().color(p.error)).wrap());
                }
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if theme::secondary_button(ui, tr("Copy SQL")).clicked() {
                        ui.ctx().copy_text(self.preview_text());
                    }
                    if self.running {
                        ui.add_space(8.0);
                        ui.add(egui::Spinner::new().size(14.0).color(p.muted));
                        ui.label(
                            RichText::new(if edit {
                                tr("Saving…")
                            } else {
                                tr("Creating…")
                            })
                            .color(p.muted),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        ui.add_enabled_ui(!busy, |ui| {
                            if theme::primary_button(
                                ui,
                                if edit { tr("Save") } else { tr("Create") },
                            )
                            .clicked()
                            {
                                match self.validate() {
                                    Ok(statements) => result = Some(ViewResult::Run(statements)),
                                    Err(e) => self.error = Some(e),
                                }
                            }
                        });
                        ui.add_enabled_ui(!self.running, |ui| {
                            if theme::secondary_button(ui, tr("Cancel")).clicked() {
                                result = Some(ViewResult::Cancel);
                            }
                        });
                    });
                });
            });
        if result.is_none() && !self.running && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            result = Some(ViewResult::Cancel);
        }
        result
    }

    fn form(&mut self, ui: &mut Ui, schemas: &[String]) {
        let driver = self.driver;
        let edit = self.is_edit();
        let spec = &mut self.spec;
        egui::Grid::new("view_form")
            .num_columns(2)
            .spacing([14.0, 10.0])
            .min_col_width(80.0)
            .show(ui, |ui| {
                ui.label(tr("Name:"));
                let name = ui.add(
                    egui::TextEdit::singleline(&mut spec.name)
                        .margin(vec2(8.0, 5.0))
                        .desired_width(f32::INFINITY),
                );
                if std::mem::take(&mut self.focus_name) {
                    name.request_focus();
                }
                ui.end_row();
                if !driver.flat_schema() {
                    ui.label(tr("Schema:"));
                    // A view is renamed within its schema: editing doesn't move it.
                    ui.add_enabled_ui(!edit, |ui| {
                        if schemas.is_empty() || edit {
                            ui.add(
                                egui::TextEdit::singleline(&mut spec.schema)
                                    .margin(vec2(8.0, 5.0))
                                    .desired_width(220.0),
                            );
                        } else {
                            egui::ComboBox::from_id_salt("view_schema")
                                .selected_text(spec.schema.as_str())
                                .width(220.0)
                                .show_ui(ui, |ui| {
                                    for s in schemas {
                                        ui.selectable_value(&mut spec.schema, s.clone(), s);
                                    }
                                });
                        }
                    });
                    ui.end_row();
                }
                ui.label(tr("Columns:"));
                ui.add(
                    egui::TextEdit::singleline(&mut spec.columns)
                        .font(egui::TextStyle::Monospace)
                        .hint_text(tr("Optional, e.g. id, name"))
                        .margin(vec2(8.0, 5.0))
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();
                let mysql = matches!(driver, Driver::MySql | Driver::MariaDb);
                if !edit || driver == Driver::Oracle || mysql {
                    ui.label(tr("Options:"));
                    ui.horizontal(|ui| {
                        if !edit {
                            ui.checkbox(
                                &mut spec.or_replace,
                                RichText::new(views::replace_label(driver)).monospace(),
                            );
                        }
                        if driver == Driver::Oracle {
                            ui.checkbox(&mut spec.force, RichText::new("FORCE").monospace());
                        }
                        if mysql {
                            ui.add_space(8.0);
                            option_combo(
                                ui,
                                "ALGORITHM",
                                "view_algorithm",
                                &mut spec.algorithm,
                                &ALGORITHMS,
                            );
                            ui.add_space(8.0);
                            option_combo(
                                ui,
                                "SQL SECURITY",
                                "view_security",
                                &mut spec.security,
                                &SECURITIES,
                            );
                        }
                    });
                    ui.end_row();
                }
            });
    }

    fn editor(&mut self, ui: &mut Ui) {
        let p = theme::pal(ui);
        let dark = ui.visuals().dark_mode;
        let driver = self.driver;
        egui::Frame::NONE
            .fill(p.editor_bg)
            .stroke(egui::Stroke::new(1.0, p.control_border))
            .corner_radius(6)
            .inner_margin(Margin::same(8))
            .show(ui, |ui| {
                egui::ScrollArea::both()
                    .id_salt("view_editor")
                    .max_height(240.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, _w: f32| {
                            let mut job = sql::highlight_for(
                                buf.as_str(),
                                driver,
                                dark,
                                FontId::monospace(13.0),
                            );
                            job.wrap.max_width = f32::INFINITY;
                            ui.fonts_mut(|f| f.layout_job(job))
                        };
                        ui.add(
                            egui::TextEdit::multiline(&mut self.spec.body)
                                .id(egui::Id::new("view_editor_sql"))
                                .code_editor()
                                .frame(egui::Frame::NONE)
                                .desired_width(f32::INFINITY)
                                .desired_rows(10)
                                .layouter(&mut layouter),
                        );
                    });
            });
    }

    fn preview(&self, ui: &mut Ui) {
        let p = theme::pal(ui);
        let dark = ui.visuals().dark_mode;
        let text = self.preview_text();
        let driver = self.driver;
        egui::Frame::NONE
            .fill(p.editor_bg)
            .stroke(egui::Stroke::new(1.0, p.control_border))
            .corner_radius(6)
            .inner_margin(Margin::same(8))
            .show(ui, |ui| {
                egui::ScrollArea::both()
                    .id_salt("view_preview")
                    .max_height(150.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, _w: f32| {
                            let mut job = sql::highlight_for(
                                buf.as_str(),
                                driver,
                                dark,
                                FontId::monospace(12.5),
                            );
                            job.wrap.max_width = f32::INFINITY;
                            ui.fonts_mut(|f| f.layout_job(job))
                        };
                        let mut t = text.as_str();
                        ui.add(
                            egui::TextEdit::multiline(&mut t)
                                .id(egui::Id::new("view_preview_sql"))
                                .code_editor()
                                .frame(egui::Frame::NONE)
                                .desired_width(f32::INFINITY)
                                .desired_rows(2)
                                .layouter(&mut layouter),
                        );
                    });
            });
    }
}

/// MySQL option with a "Default" (not written) choice.
fn option_combo(ui: &mut Ui, label: &str, id: &str, value: &mut String, choices: &[&str]) {
    let shown = |v: &str| {
        if v.is_empty() {
            tr("Default").to_string()
        } else {
            v.to_string()
        }
    };
    ui.label(RichText::new(label).monospace());
    egui::ComboBox::from_id_salt(id)
        .selected_text(shown(value))
        .width(110.0)
        .show_ui(ui, |ui| {
            for c in choices {
                ui.selectable_value(value, (*c).to_string(), shown(c));
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_and_loaded_definitions() {
        let mut d = ViewDialog::new_view(1, Driver::Postgres, "db".into(), "public".into());
        assert!(d.validate().is_err(), "name required");
        d.spec.name = "v".into();
        d.spec.body = " ; ".into();
        assert!(d.validate().is_err(), "query required");
        d.spec.body = "select 1".into();
        assert_eq!(
            d.validate().unwrap().statements,
            vec!["create view public.v as\nselect 1"]
        );

        let mut e = ViewDialog::edit_view(
            1,
            Driver::MariaDb,
            "shop".into(),
            "shop".into(),
            "v".into(),
            7,
        );
        assert!(e.is_edit() && e.loading == Some(7));
        e.definition_loaded(Ok(ViewDefinition {
            body: "select 2".into(),
            algorithm: "MERGE".into(),
            ..Default::default()
        }));
        assert!(e.loading.is_none());
        assert_eq!(
            e.validate().unwrap().statements,
            vec!["create or replace algorithm = merge view shop.v as\nselect 2"]
        );
        e.definition_loaded(Err("boom".into()));
        assert_eq!(e.error.as_deref(), Some("boom"));
    }
}
