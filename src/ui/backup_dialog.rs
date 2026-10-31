//! Database backup settings, native capabilities and a non-blocking job status.
use crate::db::{
    Driver,
    backup::{BackupMode, BackupRecord, BackupRequest},
};
use crate::i18n::{tr, trf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub struct BackupDialog {
    pub id: uuid::Uuid,
    pub conn: u64,
    pub request: BackupRequest,
    pub running: bool,
    pub cancel: Arc<AtomicBool>,
    pub result: Option<Result<String, String>>,
    driver: Driver,
    read_only: bool,
}

pub enum BackupAction {
    Start(BackupRequest),
    Close,
}

impl BackupDialog {
    pub fn new(conn: u64, driver: Driver, db: String, read_only: bool) -> Self {
        let mut request = BackupRequest::new(db.clone(), driver);
        let name: String = db
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .take(80)
            .collect();
        let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let extension = match driver {
            Driver::Sqlite => "sqlite",
            Driver::Postgres => "dump",
            Driver::Oracle => "dmp",
            Driver::MsSql => "bak",
            _ => "sql",
        };
        request.destination = if BackupRequest::local(driver) {
            dirs::document_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join(format!("{name}_{stamp}.{extension}"))
                .to_string_lossy()
                .into_owned()
        } else if driver == Driver::Oracle {
            format!("{name}_{stamp}.dmp")
        } else if driver == Driver::Snowflake {
            format!("BACKUP_{stamp}")
        } else {
            String::new()
        };
        Self {
            id: uuid::Uuid::new_v4(),
            conn,
            request,
            running: false,
            cancel: Arc::new(AtomicBool::new(false)),
            result: None,
            driver,
            read_only,
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, history: &[BackupRecord]) -> Option<BackupAction> {
        let mut action = None;
        let subtitle = format!("{} · {}", self.driver.label(), self.request.db);
        super::workflow_dialog::show(ctx, egui::Id::new("database_backup"), tr("Back up database"), &subtitle, egui::vec2(680.0, 620.0), |root, regions| {
            let mut body = regions.body_ui(root);
            egui::ScrollArea::vertical().id_salt("backup_body").auto_shrink([false, false]).show(&mut body, |ui| {
                    ui.add_enabled_ui(!self.running, |ui| {
                        let before = self.request.mode;
                        ui.horizontal(|ui| {
                            ui.label(tr("Backup type"));
                            egui::ComboBox::from_id_salt("backup_mode").selected_text(self.request.mode.label()).show_ui(ui, |ui| {
                                for &mode in BackupMode::for_driver(self.driver) { ui.selectable_value(&mut self.request.mode, mode, mode.label()); }
                            });
                        });
                        if self.driver == Driver::MsSql && before != self.request.mode { self.request.copy_only = self.request.mode != BackupMode::Differential; }
                        let label = match self.driver {
                            Driver::MsSql => tr("File path on database server"),
                            Driver::Oracle => tr("Dump filename on database server"),
                            Driver::Snowflake => tr("Backup set name"),
                            _ => tr("Backup file path"),
                        };
                        ui.label(label);
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut self.request.destination).desired_width((ui.available_width() - 90.0).max(40.0)));
                            if BackupRequest::local(self.driver) && ui.button(tr("Browse…")).clicked() {
                                let path = std::path::Path::new(&self.request.destination);
                                let mut dialog = rfd::FileDialog::new().set_file_name(path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
                                if let Some(parent) = path.parent() { dialog = dialog.set_directory(parent); }
                                if let Some(path) = dialog.save_file() { self.request.destination = path.to_string_lossy().into_owned(); }
                            }
                        });
                        ui.collapsing(tr("Options"), |ui| {
                            match self.driver {
                                Driver::Postgres | Driver::MySql | Driver::MariaDb => {
                                    ui.label(tr("Backup utility"));
                                    ui.horizontal(|ui| {
                                        ui.add(egui::TextEdit::singleline(&mut self.request.utility).desired_width((ui.available_width() - 90.0).max(40.0)));
                                        if ui.button(tr("Browse…")).clicked() && let Some(path) = rfd::FileDialog::new().pick_file() { self.request.utility = path.to_string_lossy().into_owned(); }
                                    });
                                    ui.checkbox(&mut self.request.compress, tr("Compress backup"));
                                    if self.driver == Driver::Postgres { ui.checkbox(&mut self.request.archive, tr("Custom archive format (pg_restore)")); }
                                    else {
                                        ui.checkbox(&mut self.request.include_objects, tr("Include routines, events and triggers"));
                                        ui.checkbox(&mut self.request.transaction, tr("Consistent transaction (InnoDB)"));
                                        ui.checkbox(&mut self.request.trust_cert, tr("Trust server certificate"));
                                        ui.label(tr("Disable transaction mode to lock tables. Non-transactional tables need a locking backup."));
                                    }
                                    ui.label(tr("Logical dumps contain schema and data, not binary or transaction log files."));
                                }
                                Driver::MsSql => {
                                    ui.add_enabled(self.request.mode != BackupMode::Differential, egui::Checkbox::new(&mut self.request.copy_only, tr("Copy only (preserve the backup chain)")));
                                    ui.checkbox(&mut self.request.checksum, tr("Verify checksums")); ui.checkbox(&mut self.request.compress, tr("Compress backup"));
                                    ui.label(tr("The path is on SQL Server. Existing backup files are appended to, never overwritten."));
                                    ui.label(tr("Full backups include the recovery log required for restore."));
                                }
                                Driver::Oracle => {
                                    ui.label(tr("Oracle directory object")); ui.text_edit_singleline(&mut self.request.directory);
                                    ui.checkbox(&mut self.request.export_log, tr("Create a unique export log file"));
                                    ui.label(tr("Data Pump full export requires export privileges. Files are written to the server directory object, not this computer."));
                                }
                                Driver::Snowflake => {
                                    ui.label(tr("Schema for the backup set")); ui.text_edit_singleline(&mut self.request.directory);
                                    ui.label(tr("Creates a new native backup set and adds a database backup. Requires backup-set privileges; no automatic expiry is configured."));
                                }
                                Driver::Sqlite => { ui.label(tr("Creates a consistent SQLite file including committed WAL data, without copying log files.")); }
                                _ => {}
                            }
                        });
                        if !BackupRequest::local(self.driver) {
                            ui.collapsing(tr("SQL preview"), |ui| {
                                if let Ok(sql) = self.request.sql(self.driver, "DATACRAFT_BACKUP_JOB") { ui.add(egui::Label::new(egui::RichText::new(sql.join("\n")).monospace()).wrap()); }
                            });
                        }
                    });
                    if let Some(result) = &self.result {
                        match result {
                            Ok(message) => { ui.colored_label(ui.visuals().hyperlink_color, message); }
                            Err(error) => { ui.add(egui::Label::new(egui::RichText::new(error).color(ui.visuals().error_fg_color)).wrap()); }
                        }
                    }
                    if self.read_only && !BackupRequest::local(self.driver) { ui.label(tr("This connection is read-only")); }
                    if !BackupRequest::local(self.driver) { ui.label(tr("After interruption, verify the server-side backup job and destination before retrying.")); }
                    ui.collapsing(tr("Previous backups"), |ui| {
                        egui::ScrollArea::vertical().max_height(120.0).show(ui, |ui| {
                            for record in history.iter().rev().filter(|r| r.conn == self.conn && r.db == self.request.db) {
                                ui.horizontal(|ui| { ui.label(&record.created); ui.label(record.mode.label()); if ui.small_button(tr("Copy path")).clicked() { ui.ctx().copy_text(record.destination.clone()); } });
                                ui.add(egui::Label::new(&record.destination).truncate());
                            }
                        });
                    });
                });
            let mut footer = regions.footer_ui(root);
            if self.running {
                footer.horizontal(|ui| {
                    ui.spinner();
                    ui.label(tr("Creating backup…"));
                });
            }
            footer.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let validation = self.request.validate(self.driver);
                let valid = validation.is_ok() && !(self.read_only && !BackupRequest::local(self.driver));
                let start = ui.add_enabled_ui(!self.running && valid, |ui| crate::ui::theme::primary_button(ui, tr("Start backup"))).inner;
                let start = if let Err(error) = validation { start.on_disabled_hover_text(error.to_string()) } else { start };
                if start.clicked() {
                    self.cancel = Arc::new(AtomicBool::new(false));
                    self.running = true;
                    self.result = None;
                    action = Some(BackupAction::Start(self.request.clone()));
                }
                if self.running {
                    if crate::ui::theme::secondary_button(ui, tr("Cancel")).clicked() {
                        self.cancel.store(true, Ordering::Relaxed);
                    }
                } else if crate::ui::theme::secondary_button(ui, tr("Close")).clicked() {
                    action = Some(BackupAction::Close);
                }
            });
        });
        action
    }
    pub fn completed(&mut self, result: &Result<Option<u64>, String>) {
        self.running = false;
        self.result = Some(
            result
                .as_ref()
                .map(|_| {
                    trf(
                        "Backup created: {path}",
                        &[("path", &self.request.destination)],
                    )
                })
                .map_err(Clone::clone),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme;
    use egui_kittest::kittest::Queryable;

    #[test]
    fn footer_actions_stay_visible_with_long_backup_errors() {
        for size in [egui::vec2(780.0, 560.0), egui::vec2(400.0, 300.0)] {
            for dark in [false, true] {
                let mut dialog = BackupDialog::new(1, Driver::MariaDb, "shop_".repeat(50), false);
                dialog.result = Some(Err("Backup failed with additional diagnostic context.\n".repeat(80)));
                let mut first = true;
                let mut h = egui_kittest::Harness::builder().with_size(size).build_ui_state(
                    move |ui, dialog: &mut BackupDialog| {
                        if std::mem::take(&mut first) {
                            theme::install_fonts(ui.ctx());
                            theme::apply(ui.ctx(), dark);
                        } else {
                            dialog.show(ui.ctx(), &[]);
                        }
                    },
                    dialog,
                );
                h.run_steps(3);
                let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
                for label in ["Start backup", "Close"] {
                    assert!(screen.contains_rect(h.get_by_label(label).rect()), "{label}");
                }
                h.state_mut().running = true;
                h.run_steps(2);
                assert!(screen.contains_rect(h.get_by_label("Cancel").rect()));
                h.get_by_label("Cancel").click();
                h.run_steps(2);
                assert!(h.state().cancel.load(Ordering::Relaxed));
                assert!(h.state().running);
            }
        }
    }

    #[test]
    fn every_sql_engine_exposes_its_native_backup_settings_without_starting_a_job() {
        for driver in [
            Driver::Postgres,
            Driver::MySql,
            Driver::MariaDb,
            Driver::Sqlite,
            Driver::MsSql,
            Driver::Oracle,
            Driver::Snowflake,
        ] {
            let mut first = true;
            let mut h = egui_kittest::Harness::builder()
                .with_size(egui::vec2(1000.0, 750.0))
                .build_ui_state(
                    move |ui, dialog: &mut BackupDialog| {
                        if std::mem::take(&mut first) {
                            theme::install_fonts(ui.ctx());
                            theme::apply(ui.ctx(), true);
                            return;
                        }
                        assert!(dialog.show(ui.ctx(), &[]).is_none());
                    },
                    BackupDialog::new(1, driver, "shop".into(), false),
                );
            h.run_steps(3);
            assert!(h.query_by_label("Backup type").is_some());
            assert!(h.query_by_label("Start backup").is_some());
            assert!(!h.state().running);
        }
    }

    #[test]
    #[ignore = "Requires a GPU; renders native backup options without accessing a database"]
    fn render_backup_settings() {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/workspace-snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        for dark in [false, true] {
            for driver in [Driver::MariaDb, Driver::MsSql, Driver::Oracle, Driver::Snowflake] {
                let mut dialog = BackupDialog::new(1, driver, "shop".into(), false);
                dialog.request.destination = match driver {
                    Driver::MsSql => "D:\\Backups\\shop.bak",
                    Driver::Oracle => "shop.dmp",
                    Driver::Snowflake => "SHOP_BACKUP",
                    _ => "C:\\Backups\\shop.sql",
                }
                .into();
                if driver == Driver::Oracle {
                    dialog.request.directory = "DATA_PUMP_DIR".into();
                }
                let mut first = true;
                let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(780.0, 560.0)).build_ui_state(
                    move |ui, dialog: &mut BackupDialog| {
                        if std::mem::take(&mut first) {
                            theme::install_fonts(ui.ctx());
                            theme::apply(ui.ctx(), dark);
                            return;
                        }
                        dialog.show(ui.ctx(), &[]);
                    },
                    dialog,
                );
                h.run_steps(3);
                h.get_by_label("Options").click();
                h.run_steps(3);
                h.render().unwrap().save(dir.join(format!("backup-{}-{}.png", driver.label().replace('/', "-"), if dark { "dark" } else { "light" }))).unwrap();
            }
        }
    }
}
