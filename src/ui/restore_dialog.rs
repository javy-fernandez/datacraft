//! Database restore: inspect a backup, choose the target and native options, then a background job.
use crate::db::{
    Driver,
    restore::{self, RestoreContent, RestoreFormat, RestorePreview, RestoreProgress, RestoreRecord, RestoreRequest},
};
use crate::i18n::{tr, trf};
use crate::ui::theme::{self, palette};
use egui::RichText;
use std::sync::{Arc, atomic::Ordering};

pub struct RestoreDialog {
    pub id: uuid::Uuid,
    pub conn: u64,
    pub request: RestoreRequest,
    pub running: bool,
    pub inspecting: bool,
    pub progress: Arc<RestoreProgress>,
    pub preview: Option<Result<RestorePreview, String>>,
    pub result: Option<Result<String, String>>,
    /// The overwrite confirmation is open.
    pub confirm: bool,
    /// Inspect the prefilled source on the first frame.
    pub auto_inspect: bool,
    driver: Driver,
    read_only: bool,
    databases: Vec<String>,
    /// Source the preview belongs to; editing it discards the preview.
    inspected: String,
    utility_edited: bool,
    /// SQL Server: database name inside the backup set, and the target the file moves were made for.
    source_db: String,
    moves_for: String,
}

pub enum RestoreAction {
    Inspect(RestoreRequest),
    Start(RestoreRequest),
    Close,
}

impl RestoreDialog {
    pub fn new(conn: u64, driver: Driver, db: Option<String>, databases: Vec<String>, read_only: bool) -> Self {
        let mut request = RestoreRequest::new(db.clone().unwrap_or_default());
        request.create = db.is_none() && driver != Driver::MsSql;
        Self {
            id: uuid::Uuid::new_v4(),
            conn,
            request,
            running: false,
            inspecting: false,
            progress: Arc::new(RestoreProgress::default()),
            preview: None,
            result: None,
            confirm: false,
            auto_inspect: false,
            driver,
            read_only,
            databases,
            inspected: String::new(),
            utility_edited: false,
            source_db: String::new(),
            moves_for: String::new(),
        }
    }

    pub fn target_exists(&self) -> bool {
        let target = self.request.db.trim();
        self.databases.iter().any(|d| if self.driver == Driver::MsSql { d.eq_ignore_ascii_case(target) } else { d == target })
    }

    fn validate(&self) -> Result<(), String> {
        self.request.validate(self.driver).map_err(|e| e.to_string())?;
        if self.request.create && self.target_exists() {
            return Err(tr("A database with this name already exists").into());
        }
        if self.read_only {
            return Err(tr("This connection is read-only").into());
        }
        Ok(())
    }

    fn inspect(&mut self) -> RestoreAction {
        self.inspecting = true;
        self.inspected = self.request.source.clone();
        self.result = None;
        RestoreAction::Inspect(self.request.clone())
    }

    pub fn inspected(&mut self, result: Result<RestorePreview, String>) {
        self.inspecting = false;
        if let Ok(preview) = &result {
            self.request.format = preview.format;
            if let Some(format) = preview.format.filter(|_| !self.utility_edited && restore::local(self.driver)) {
                self.request.utility = restore::utility_path(self.driver, format);
            }
            if !preview.format.is_some_and(|f| matches!(f, RestoreFormat::Custom | RestoreFormat::Directory)) {
                self.request.jobs = 1;
            }
            if let Some(set) = preview.sets.iter().find(|s| s.position == self.request.position).or_else(|| preview.sets.iter().rev().find(|s| s.kind == 1)) {
                self.request.position = set.position;
                self.source_db.clone_from(&set.database);
                if self.request.db.trim().is_empty() {
                    self.request.db.clone_from(&set.database);
                }
                self.request.moves = restore::default_moves(&preview.files, &self.source_db, &self.request.db);
                self.moves_for.clone_from(&self.request.db);
            }
        } else {
            self.request.format = None;
        }
        self.preview = Some(result);
    }

    pub fn show(&mut self, ctx: &egui::Context, history: &[RestoreRecord]) -> Option<RestoreAction> {
        let mut action = None;
        if std::mem::take(&mut self.auto_inspect) && !self.request.source.is_empty() {
            action = Some(self.inspect());
        }
        if self.request.source != self.inspected && !self.inspecting {
            self.preview = None;
            self.request.format = None;
        }
        if self.driver == Driver::MsSql && self.request.db != self.moves_for && let Some(Ok(preview)) = &self.preview {
            self.request.moves = restore::default_moves(&preview.files, &self.source_db, &self.request.db);
            self.moves_for.clone_from(&self.request.db);
        }
        if self.running {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
        let p = palette(ctx.global_style().visuals.dark_mode);
        let subtitle = format!("{} · {}", self.driver.label(), if self.request.db.is_empty() { tr("New database") } else { &self.request.db });
        super::workflow_dialog::show(ctx, egui::Id::new("database_restore"), tr("Restore database"), &subtitle, egui::vec2(740.0, 780.0), |root, regions| {
            let mut body = regions.body_ui(root);
            egui::ScrollArea::vertical().id_salt("restore_body").auto_shrink([false, false]).show(&mut body, |ui| {
                ui.add_enabled_ui(!self.running && !self.inspecting, |ui| {
                    let local = restore::local(self.driver);
                    ui.label(if local { tr("Backup file") } else { tr("Backup file path on database server") });
                    ui.horizontal(|ui| {
                        let buttons = if local { 260.0 } else { 90.0 };
                        ui.add(egui::TextEdit::singleline(&mut self.request.source).desired_width((ui.available_width() - buttons).max(40.0)));
                        if local && ui.button(tr("Browse…")).clicked() && let Some(path) = rfd::FileDialog::new().pick_file() {
                            self.request.source = path.to_string_lossy().into_owned();
                            action = Some(self.inspect());
                        }
                        if self.driver == Driver::Postgres && ui.button(tr("Folder…")).on_hover_text(tr("pg_dump directory archive")).clicked() && let Some(path) = rfd::FileDialog::new().pick_folder() {
                            self.request.source = path.to_string_lossy().into_owned();
                            action = Some(self.inspect());
                        }
                        if ui.add_enabled(!self.request.source.trim().is_empty(), egui::Button::new(tr("Inspect"))).clicked() {
                            action = Some(self.inspect());
                        }
                    });
                    if !local {
                        ui.colored_label(p.warn, tr("SQL Server reads this path on the database server, not on this computer."));
                    }
                    self.preview_ui(ui, &mut action);
                    ui.add_space(6.0);
                    self.target_ui(ui, &p);
                    egui::CollapsingHeader::new(tr("Options")).default_open(true).show(ui, |ui| self.options_ui(ui));
                });
                if let Some(result) = &self.result {
                    match result {
                        Ok(message) => { ui.colored_label(ui.visuals().hyperlink_color, message); }
                        Err(error) => { ui.add(egui::Label::new(RichText::new(error).color(p.error)).wrap()); }
                    }
                }
                if self.read_only { ui.label(tr("This connection is read-only")); }
                ui.collapsing(tr("Previous restores"), |ui| {
                    egui::ScrollArea::vertical().max_height(120.0).show(ui, |ui| {
                        for record in history.iter().rev().filter(|r| r.conn == self.conn) {
                            ui.horizontal(|ui| { ui.label(&record.created); ui.label(RichText::new(&record.db).strong()); if ui.small_button(tr("Copy path")).clicked() { ui.ctx().copy_text(record.source.clone()); } });
                            ui.add(egui::Label::new(&record.source).truncate());
                        }
                    });
                });
            });
            let mut footer = regions.footer_ui(root);
            if self.running || self.inspecting {
                footer.horizontal(|ui| match self.progress.fraction().filter(|_| self.running) {
                    Some(fraction) => {
                        let text = trf("Restoring… {percent}%", &[("percent", &((fraction * 100.0).round() as u32))]);
                        ui.visuals_mut().extreme_bg_color = p.hover;
                        ui.add(egui::ProgressBar::new(fraction).desired_width(260.0).fill(p.selection).text(RichText::new(text).color(p.text)));
                    }
                    None => {
                        ui.spinner();
                        ui.label(if self.inspecting { tr("Reading backup…") } else { tr("Restoring…") });
                    }
                });
            }
            footer.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let validation = self.validate();
                let start = ui.add_enabled_ui(!self.running && !self.inspecting && validation.is_ok(), |ui| theme::primary_button(ui, tr("Start restore"))).inner;
                let start = if let Err(error) = &validation { start.on_disabled_hover_text(error) } else { start };
                if start.clicked() {
                    if self.target_exists() { self.confirm = true; } else { action = Some(self.start()); }
                }
                if self.running {
                    if theme::secondary_button(ui, tr("Cancel")).clicked() {
                        self.progress.cancel.store(true, Ordering::Relaxed);
                    }
                } else if theme::secondary_button(ui, tr("Close")).clicked() {
                    action = Some(RestoreAction::Close);
                }
            });
        });
        if self.confirm {
            self.confirm_ui(ctx, &p, &mut action);
        }
        action
    }

    fn start(&mut self) -> RestoreAction {
        self.progress = Arc::new(RestoreProgress::default());
        self.running = true;
        self.confirm = false;
        self.result = None;
        RestoreAction::Start(self.request.clone())
    }

    fn preview_ui(&mut self, ui: &mut egui::Ui, action: &mut Option<RestoreAction>) {
        let Some(preview) = &self.preview else {
            if !self.inspecting { ui.label(RichText::new(tr("Inspect the backup to see its contents before restoring.")).weak()); }
            return;
        };
        let preview = match preview {
            Ok(preview) => preview.clone(),
            Err(error) => {
                ui.add(egui::Label::new(RichText::new(error).color(ui.visuals().error_fg_color)).wrap());
                return;
            }
        };
        ui.add_space(4.0);
        let mut summary = preview.format.map(|f| f.label().to_string()).unwrap_or_default();
        if let Some(size) = preview.size { summary += &format!(" · {}", crate::ui::object_browser::format_bytes(size)); }
        ui.label(RichText::new(summary).strong());
        egui::Grid::new("restore_details").num_columns(2).spacing([12.0, 2.0]).show(ui, |ui| {
            for (key, value) in preview.details.iter().take(8) {
                ui.label(RichText::new(key).weak());
                ui.add(egui::Label::new(value).truncate());
                ui.end_row();
            }
        });
        if !preview.sets.is_empty() {
            let label = |s: &restore::BackupSet| format!("{} · {} · {} · {}", s.position, s.kind_label(), s.database, s.started);
            let selected = preview.sets.iter().find(|s| s.position == self.request.position).map(label).unwrap_or_default();
            ui.horizontal(|ui| {
                ui.label(tr("Backup set"));
                egui::ComboBox::from_id_salt("restore_set").selected_text(selected).width(ui.available_width().min(460.0)).show_ui(ui, |ui| {
                    for set in &preview.sets {
                        let response = ui.add_enabled(set.kind == 1, egui::Button::selectable(set.position == self.request.position, label(set)));
                        if response.clicked() && set.position != self.request.position {
                            self.request.position = set.position;
                            *action = Some(self.inspect());
                        }
                    }
                });
            });
        }
        if !preview.files.is_empty() {
            ui.label(RichText::new(tr("Database files on the server")).strong());
            egui::Grid::new("restore_moves").num_columns(3).spacing([10.0, 4.0]).show(ui, |ui| {
                for (file, (logical, physical)) in preview.files.iter().zip(self.request.moves.iter_mut()) {
                    ui.label(logical.as_str());
                    ui.label(RichText::new(&file.kind).weak());
                    ui.add(egui::TextEdit::singleline(physical).desired_width((ui.available_width()).max(200.0)));
                    ui.end_row();
                }
            });
        }
        if !preview.entries.is_empty() {
            let title = if preview.format.is_some_and(RestoreFormat::archive) {
                trf("Contents ({count} entries)", &[("count", &preview.entries.len())])
            } else {
                tr("First lines").to_string()
            };
            ui.collapsing(title, |ui| {
                egui::ScrollArea::vertical().id_salt("restore_entries").max_height(140.0).show(ui, |ui| {
                    for entry in &preview.entries { ui.add(egui::Label::new(RichText::new(entry).monospace().size(12.0)).truncate()); }
                });
            });
        }
    }

    fn target_ui(&mut self, ui: &mut egui::Ui, p: &theme::Palette) {
        ui.label(RichText::new(tr("Target database")).strong());
        if self.driver == Driver::MsSql {
            ui.add(egui::TextEdit::singleline(&mut self.request.db).desired_width(f32::INFINITY));
        } else {
            ui.horizontal(|ui| {
                if ui.radio(!self.request.create, tr("Existing database")).clicked() && self.request.create {
                    self.request.create = false;
                    if !self.target_exists() { self.request.db = self.databases.first().cloned().unwrap_or_default(); }
                }
                if ui.radio(self.request.create, tr("Create new database")).clicked() && !self.request.create {
                    self.request.create = true;
                    self.request.db.clear();
                }
            });
            if self.request.create {
                ui.add(egui::TextEdit::singleline(&mut self.request.db).hint_text(tr("New database name")).desired_width(f32::INFINITY));
            } else {
                egui::ComboBox::from_id_salt("restore_target").selected_text(self.request.db.as_str()).width(ui.available_width().min(360.0)).show_ui(ui, |ui| {
                    for db in &self.databases { ui.selectable_value(&mut self.request.db, db.clone(), db); }
                });
            }
        }
        if self.request.db.trim().is_empty() {
            return;
        }
        if self.target_exists() {
            let warning = if self.driver == Driver::MsSql {
                tr("This database exists and will be overwritten by the backup.")
            } else {
                tr("The backup is restored into this existing database.")
            };
            ui.colored_label(if self.request.create || self.driver == Driver::MsSql { p.error } else { p.warn }, warning);
        } else if self.driver == Driver::MsSql || self.request.create {
            ui.label(RichText::new(tr("A new database will be created.")).weak());
        }
    }

    fn options_ui(&mut self, ui: &mut egui::Ui) {
        let format = self.request.format;
        if restore::local(self.driver) {
            ui.label(tr("Restore utility"));
            ui.horizontal(|ui| {
                if ui.add(egui::TextEdit::singleline(&mut self.request.utility).desired_width((ui.available_width() - 90.0).max(40.0))).changed() { self.utility_edited = true; }
                if ui.button(tr("Browse…")).clicked() && let Some(path) = rfd::FileDialog::new().pick_file() {
                    self.request.utility = path.to_string_lossy().into_owned();
                    self.utility_edited = true;
                }
            });
        }
        match self.driver {
            Driver::Postgres if format.is_some_and(RestoreFormat::archive) => {
                ui.horizontal(|ui| {
                    ui.label(tr("Contents to restore"));
                    egui::ComboBox::from_id_salt("restore_content").selected_text(self.request.content.label()).show_ui(ui, |ui| {
                        for content in [RestoreContent::All, RestoreContent::Schema, RestoreContent::Data] { ui.selectable_value(&mut self.request.content, content, content.label()); }
                    });
                });
                ui.add_enabled(self.request.content != RestoreContent::Data, egui::Checkbox::new(&mut self.request.clean, tr("Drop existing objects first (--clean --if-exists)")));
                ui.checkbox(&mut self.request.no_owner, tr("Skip object ownership (--no-owner)"));
                ui.add_enabled_ui(matches!(format, Some(RestoreFormat::Custom | RestoreFormat::Directory)), |ui| {
                    ui.horizontal(|ui| { ui.label(tr("Parallel jobs")); ui.add(egui::DragValue::new(&mut self.request.jobs).range(1..=16)); });
                });
                ui.label(tr("pg_restore stops at the first error. Parallel jobs restore custom and directory archives faster."));
            }
            Driver::Postgres => { ui.label(tr("Plain SQL dumps run with psql and stop at the first error; statements that already ran are kept.")); }
            Driver::MySql | Driver::MariaDb => {
                ui.checkbox(&mut self.request.trust_cert, tr("Trust server certificate"));
                ui.label(tr("The client stops at the first error. CREATE DATABASE and USE lines in the dump are skipped so the target database receives the restore."));
            }
            Driver::MsSql => {
                ui.add_enabled(self.target_exists(), egui::Checkbox::new(&mut self.request.replace, tr("Overwrite the existing database (WITH REPLACE)")));
                ui.add_enabled(self.target_exists(), egui::Checkbox::new(&mut self.request.single_user, tr("Close other connections to the target first")));
                ui.label(tr("Only full database backups are restored, with recovery. Data and log files are created at the paths above, on the server."));
                if !self.target_exists() { self.request.replace = false; self.request.single_user = false; }
                ui.collapsing(tr("SQL preview"), |ui| {
                    if let Ok(sql) = self.request.mssql_sql() { ui.add(egui::Label::new(RichText::new(sql).monospace()).wrap()); }
                });
            }
            _ => {}
        }
    }

    fn confirm_ui(&mut self, ctx: &egui::Context, p: &theme::Palette, action: &mut Option<RestoreAction>) {
        let frame = egui::Frame::popup(&ctx.global_style()).fill(p.popup_bg).corner_radius(12).inner_margin(egui::Margin::same(20)).stroke(egui::Stroke::new(1.0, p.popup_border));
        egui::Modal::new(egui::Id::new("confirm_restore")).frame(frame).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.label(RichText::new(tr("Overwrite existing database?")).font(theme::bold(15.0)).color(p.text));
            ui.add_space(8.0);
            let question = if self.driver == Driver::MsSql {
                trf("“{db}” will be replaced by the database in the backup.", &[("db", &self.request.db)])
            } else if self.request.clean {
                trf("Objects in “{db}” that the backup contains are dropped and recreated from the backup.", &[("db", &self.request.db)])
            } else {
                trf("The backup's objects and rows are added to “{db}”.", &[("db", &self.request.db)])
            };
            ui.label(question);
            ui.label(RichText::new(tr("This action cannot be undone.")).color(p.error));
            ui.add_space(16.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let restore = egui::Button::new(RichText::new(tr("Restore")).color(egui::Color32::WHITE)).fill(p.error).stroke(egui::Stroke::new(1.0, p.error)).corner_radius(6).min_size(egui::vec2(72.0, 28.0));
                if ui.add(restore).clicked() { *action = Some(self.start()); }
                if theme::secondary_button(ui, tr("Cancel")).clicked() { self.confirm = false; }
            });
        });
    }

    pub fn completed(&mut self, result: &Result<(), String>) {
        self.running = false;
        self.result = Some(result.as_ref().map(|_| trf("Database “{db}” restored.", &[("db", &self.request.db)])).map_err(Clone::clone));
        if result.is_ok() && !self.target_exists() {
            self.databases.push(self.request.db.clone());
            self.request.create = false;
        }
    }
}

/// Inspected samples for the dialog tests and the documentation screenshots.
#[cfg(test)]
impl RestoreDialog {
    pub(crate) fn sample_sql_server() -> RestoreDialog {
        let mut dialog = RestoreDialog::new(1, Driver::MsSql, Some("shop".into()), vec!["master".into(), "shop".into()], false);
        dialog.request.source = "D:\\Backups\\shop_20261101.bak".into();
        dialog.inspected = dialog.request.source.clone();
        dialog.inspected(Ok(RestorePreview {
            format: Some(RestoreFormat::MsSql),
            details: vec![("Database".into(), "shop".into()), ("Server".into(), "SQL01".into()), ("Created".into(), "2026-11-01 22:00:04".into())],
            sets: vec![
                restore::BackupSet { position: 1, kind: 1, database: "shop".into(), server: "SQL01".into(), started: "2026-11-01 22:00:04".into() },
                restore::BackupSet { position: 2, kind: 2, database: "shop".into(), server: "SQL01".into(), started: "2026-11-01 23:00:00".into() },
            ],
            files: vec![
                restore::BackupFile { logical: "shop".into(), physical: "D:\\Data\\shop.mdf".into(), kind: "D".into() },
                restore::BackupFile { logical: "shop_log".into(), physical: "D:\\Data\\shop_log.ldf".into(), kind: "L".into() },
            ],
            ..Default::default()
        }));
        dialog
    }

    pub(crate) fn sample_postgres() -> RestoreDialog {
        let mut dialog = RestoreDialog::new(1, Driver::Postgres, Some("shop".into()), vec!["postgres".into(), "shop".into()], false);
        dialog.request.source = "C:\\Backups\\shop_20261101.dump".into();
        dialog.request.utility = "C:\\Program Files\\PostgreSQL\\16\\bin\\pg_restore.exe".into();
        dialog.utility_edited = true;
        dialog.inspected = dialog.request.source.clone();
        dialog.inspected(Ok(RestorePreview {
            format: Some(RestoreFormat::Custom),
            size: Some(18_432_118),
            details: vec![("dbname".into(), "shop".into()), ("TOC Entries".into(), "48".into()), ("Dumped from database version".into(), "16.4".into()), ("Archive created at".into(), "2026-11-01 22:00:04 CET".into())],
            entries: ["SCHEMA - sales postgres", "TABLE public customers postgres", "TABLE public orders postgres", "TABLE DATA public customers postgres", "TABLE DATA public orders postgres", "CONSTRAINT public orders orders_pkey postgres", "FK CONSTRAINT public orders orders_customer_fkey postgres"].map(String::from).to_vec(),
            ..Default::default()
        }));
        dialog
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::Queryable;

    fn harness(dialog: RestoreDialog, size: egui::Vec2, dark: bool) -> egui_kittest::Harness<'static, (RestoreDialog, Vec<String>)> {
        let mut first = true;
        egui_kittest::Harness::builder().with_size(size).build_ui_state(
            move |ui, state: &mut (RestoreDialog, Vec<String>)| {
                if std::mem::take(&mut first) {
                    theme::install_fonts(ui.ctx());
                    theme::apply(ui.ctx(), dark);
                    return;
                }
                match state.0.show(ui.ctx(), &[]) {
                    Some(RestoreAction::Inspect(_)) => state.1.push("inspect".into()),
                    Some(RestoreAction::Start(request)) => state.1.push(format!("start {}", request.db)),
                    Some(RestoreAction::Close) => state.1.push("close".into()),
                    None => {}
                }
            },
            (dialog, Vec::new()),
        )
    }

    #[test]
    fn existing_targets_need_confirmation_and_new_databases_start_directly() {
        let mut dialog = RestoreDialog::sample_postgres();
        dialog.request.create = true;
        dialog.request.db = "shop_copy".into();
        let mut h = harness(dialog, egui::vec2(900.0, 760.0), true);
        h.run_steps(3);
        assert!(h.query_by_label("Contents (7 entries)").is_some());
        h.get_by_label("Start restore").click();
        h.run_steps(2);
        assert_eq!(h.state().1, ["start shop_copy"]);
        assert!(h.state().0.running);
        h.state_mut().0.completed(&Ok(()));
        h.state_mut().0.request.create = false;
        h.state_mut().0.request.db = "shop".into();
        h.run_steps(2);
        h.get_by_label("Start restore").click();
        h.run_steps(2);
        assert!(h.state().0.confirm);
        assert!(h.query_by_label("Overwrite existing database?").is_some());
        h.get_by_label("Restore").click();
        h.run_steps(2);
        assert_eq!(h.state().1.last().map(String::as_str), Some("start shop"));
    }

    #[test]
    fn sql_server_copies_get_their_own_files_and_read_only_sources_cannot_start() {
        let mut dialog = RestoreDialog::sample_sql_server();
        assert_eq!(dialog.request.position, 1);
        assert_eq!(dialog.request.moves[0].1, "D:\\Data\\shop.mdf");
        dialog.request.db = "shop_test".into();
        let mut h = harness(dialog, egui::vec2(900.0, 760.0), false);
        h.run_steps(3);
        assert_eq!(h.state().0.request.moves[1].1, "D:\\Data\\shop_test_log.ldf");
        assert!(h.state().0.validate().is_ok());
        h.state_mut().0.read_only = true;
        assert!(h.state().0.validate().is_err());
        h.state_mut().0.request.source.push('x');
        h.run_steps(2);
        assert!(h.state().0.preview.is_none() && h.state().0.request.format.is_none());
    }

    #[test]
    fn footer_stays_visible_and_cancel_signals_the_job() {
        for size in [egui::vec2(780.0, 560.0), egui::vec2(420.0, 320.0)] {
            let mut dialog = RestoreDialog::sample_postgres();
            dialog.result = Some(Err("pg_restore: error: could not execute query\n".repeat(60)));
            dialog.auto_inspect = true;
            let mut h = harness(dialog, size, true);
            h.run_steps(3);
            assert_eq!(h.state().1, ["inspect"]);
            h.state_mut().0.inspected(Ok(RestorePreview { format: Some(RestoreFormat::Custom), ..Default::default() }));
            h.state_mut().0.running = true;
            h.run_steps(2);
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
            assert!(screen.contains_rect(h.get_by_label("Cancel").rect()));
            h.get_by_label("Cancel").click();
            h.run_steps(2);
            assert!(h.state().0.progress.cancel.load(Ordering::Relaxed));
        }
    }
}
