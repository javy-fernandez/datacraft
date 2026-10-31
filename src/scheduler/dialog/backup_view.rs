//! Native backup controls and asynchronous database selection for scheduled tasks.

use super::{Action, Dialog};
use crate::db::{ConnectionConfig, Driver, backup::BackupRequest};
use crate::i18n::tr;
use crate::ui::theme::Palette;
use egui::{Align, Layout, RichText, Ui, vec2};

impl Dialog {
    pub(super) fn database_picker(&mut self, ui: &mut Ui, connection: &ConnectionConfig) {
        let selected = self.backup_database(connection);
        let mut names = self.catalogs.get(&connection.id).cloned().unwrap_or_default();
        if connection.driver == Driver::Sqlite {
            names = vec!["main".into()];
        } else if connection.driver == Driver::Oracle {
            names = if connection.database.is_empty() { Vec::new() } else { vec![connection.database.clone()] };
        } else if !connection.database.is_empty() && !names.contains(&connection.database) {
            names.push(connection.database.clone());
        }
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 36.0).max(40.0);
            ui.allocate_ui_with_layout(vec2(width, 24.0), Layout::left_to_right(Align::Center), |ui| {
                egui::ComboBox::from_id_salt("backup_database")
                    .width(width)
                    .truncate()
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                    .selected_text(if selected.is_empty() { tr("Choose a database") } else { &selected })
                    .show_ui(ui, |ui| {
                        for name in &names {
                            if ui.selectable_value(&mut self.draft.database, name.clone(), name).clicked() {
                                ui.close();
                            }
                        }
                        if !matches!(connection.driver, Driver::Sqlite | Driver::Oracle) {
                            ui.separator();
                            ui.label(tr("Database"));
                            ui.text_edit_singleline(&mut self.draft.database);
                        }
                    });
            });
            if ui.add_enabled(self.catalog_receiver.is_none(), egui::Button::new("↻")).on_hover_text(tr("Load databases")).clicked() {
                self.load_catalog(ui.ctx(), connection, true);
            }
        });
        if connection.driver == Driver::Oracle && selected != connection.database {
            ui.colored_label(ui.visuals().error_fg_color, tr("Oracle backup tasks must use the saved connection service"));
        }
        if self.catalog_receiver.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.small(tr("Loading…"));
            });
        }
        if let Some((id, error)) = &self.catalog_error
            && *id == connection.id
        {
            ui.add(egui::Label::new(RichText::new(error).small().color(ui.visuals().error_fg_color)).wrap());
        }
    }

    pub(super) fn confirm_backup(&mut self, ctx: &egui::Context, connections: &[ConnectionConfig], action: &mut Option<Action>) {
        crate::ui::workflow_dialog::show(
            ctx,
            egui::Id::new("confirm_scheduled_backup"),
            tr("Confirm task"),
            tr("Full database backup"),
            vec2(680.0, 500.0),
            |root, regions| {
                let mut body = regions.body_ui(root);
                egui::ScrollArea::vertical().id_salt("backup_confirmation_body").auto_shrink([false, false]).show(&mut body, |ui| {
                    ui.label(tr("A full backup will be created automatically at the selected interval."));
                    ui.label(tr("Every run creates a new dated backup; previous backups are kept."));
                    if let Some(request) = &self.backup_preview {
                        ui.label(format!("{}: {}", tr("Database"), request.db));
                        if let Some(connection) = connections.iter().find(|c| c.id == self.draft.connection) {
                            let directory_label = match connection.driver {
                                Driver::Oracle => Some(tr("Oracle directory object")),
                                Driver::Snowflake => Some(tr("Schema for the backup set")),
                                _ => None,
                            };
                            if let Some(label) = directory_label {
                                ui.label(format!("{label}: {}", request.directory));
                            }
                        }
                        ui.label(tr("Example backup destination"));
                        ui.add(egui::Label::new(&request.destination).wrap());
                        if let Some(connection) = connections.iter().find(|c| c.id == self.draft.connection && !BackupRequest::local(c.driver)) {
                            ui.collapsing(tr("SQL preview"), |ui| {
                                if let Ok(sql) = request.sql(connection.driver, "DATACRAFT_BACKUP_JOB") {
                                    egui::ScrollArea::both().max_height(220.0).show(ui, |ui| {
                                        ui.label(egui::RichText::new(sql.join("\n")).monospace());
                                    });
                                }
                            });
                        }
                    }
                });
                let mut footer = regions.footer_ui(root);
                footer.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if crate::ui::theme::primary_button(ui, tr("Save task")).clicked() {
                        self.submit_confirmation(connections, action);
                    }
                    if crate::ui::theme::secondary_button(ui, tr("Keep editing")).clicked() {
                        self.confirm_save = false;
                    }
                });
            },
        );
    }

    pub(super) fn backup_configuration(&mut self, ui: &mut Ui, connection: &ConnectionConfig, p: &Palette) {
        let driver = connection.driver;
        let Some(backup) = self.draft.backup.as_mut() else {
            return;
        };
        ui.add_space(8.0);
        ui.label(RichText::new(tr("Full database backup")).strong());
        if BackupRequest::local(driver) || driver == Driver::MsSql {
            ui.label(if driver == Driver::MsSql { tr("Folder on database server") } else { tr("Destination folder") });
            ui.horizontal(|ui| {
                let width = if BackupRequest::local(driver) { (ui.available_width() - 94.0).max(60.0) } else { ui.available_width() };
                ui.add(egui::TextEdit::singleline(&mut backup.folder).desired_width(width));
                if BackupRequest::local(driver) && ui.button(tr("Browse…")).clicked() {
                    let mut picker = rfd::FileDialog::new();
                    if !backup.folder.is_empty() {
                        picker = picker.set_directory(&backup.folder);
                    }
                    if let Some(folder) = picker.pick_folder() {
                        backup.folder = folder.to_string_lossy().into_owned();
                    }
                }
            });
            if driver == Driver::MsSql {
                ui.label(RichText::new(tr("The folder is on SQL Server, not on this computer.")).small().color(p.muted));
            }
        }
        if driver == Driver::Oracle {
            ui.label(tr("Oracle directory object"));
            ui.add(egui::TextEdit::singleline(&mut backup.directory).desired_width(f32::INFINITY));
            ui.label(
                RichText::new(tr("Data Pump full export requires export privileges. Files are written to the server directory object, not this computer."))
                    .small()
                    .color(p.muted),
            );
        }
        if driver == Driver::Snowflake {
            ui.label(tr("Schema for the backup set"));
            ui.add(egui::TextEdit::singleline(&mut backup.directory).desired_width(f32::INFINITY));
            ui.label(
                RichText::new(tr(
                    "Creates a new native backup set and adds a database backup. Requires backup-set privileges; no automatic expiry is configured.",
                ))
                .small()
                .color(p.muted),
            );
        }
        ui.add_space(6.0);
        ui.label(RichText::new(tr("Every run creates a new dated backup; previous backups are kept.")).small().color(p.muted));
        if driver == Driver::Sqlite {
            ui.label(RichText::new(tr("Creates a consistent SQLite file including committed WAL data, without copying log files.")).small().color(p.muted));
        }
        if matches!(driver, Driver::Postgres | Driver::MySql | Driver::MariaDb | Driver::MsSql) {
            ui.add_space(8.0);
            ui.collapsing(tr("Backup options"), |ui| {
                if matches!(driver, Driver::Postgres | Driver::MySql | Driver::MariaDb) {
                    ui.label(tr("Backup utility"));
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut backup.utility).desired_width((ui.available_width() - 94.0).max(60.0)));
                        if ui.button(tr("Browse…")).clicked()
                            && let Some(path) = rfd::FileDialog::new().pick_file()
                        {
                            backup.utility = path.to_string_lossy().into_owned();
                        }
                    });
                    ui.label(RichText::new(tr("The native backup utility must be installed on this computer.")).small().color(p.muted));
                }
                ui.checkbox(&mut backup.compress, tr("Compress backup"));
                if driver == Driver::Postgres {
                    ui.checkbox(&mut backup.archive, tr("Custom archive format (pg_restore)"));
                }
                if matches!(driver, Driver::MySql | Driver::MariaDb) {
                    ui.checkbox(&mut backup.transaction, tr("Consistent transaction (InnoDB)"));
                    ui.checkbox(&mut backup.trust_cert, tr("Trust server certificate"));
                    ui.label(
                        RichText::new(tr("Disable transaction mode to lock tables. Non-transactional tables need a locking backup.")).small().color(p.muted),
                    );
                }
                if driver == Driver::MsSql {
                    ui.label(RichText::new(tr("Full backups include the recovery log required for restore.")).small().color(p.muted));
                }
            });
        }
        if connection.read_only && !BackupRequest::local(driver) {
            ui.colored_label(p.error, tr("This connection is read-only"));
        }
    }
}
