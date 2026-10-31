//! Bounded scheduler manager: only the task list and active content pane scroll.

use super::{Action, Dialog, IntervalUnit, Navigation, Tab};
use crate::db::{ConnectionConfig, Driver};
use crate::i18n::tr;
use crate::ui::theme::{self, Palette, palette};
use egui::{Align, Color32, FontId, Layout, Margin, Rect, RichText, Sense, Stroke, Ui, UiBuilder, pos2, vec2};

fn region(ui: &mut Ui, rect: Rect, id: &'static str, layout: Layout) -> Ui {
    let mut child = ui.new_child(UiBuilder::new().id_salt(id).max_rect(rect).layout(layout));
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    child
}

fn timestamp(seconds: i64) -> String {
    chrono::DateTime::from_timestamp(seconds, 0)
        .map(|time| time.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "—".into())
}

impl Dialog {
    pub(super) fn render(&mut self, ctx: &egui::Context, connections: &[ConnectionConfig], action: &mut Option<Action>) {
        let p = palette(ctx.global_style().visuals.dark_mode);
        let screen = ctx.content_rect();
        let size = vec2((screen.width() - 32.0).clamp(280.0, 1160.0), (screen.height() - 32.0).clamp(220.0, 800.0));
        let busy = self.receiver.is_some();
        let frame =
            egui::Frame::popup(&ctx.global_style()).fill(p.popup_bg).corner_radius(12).inner_margin(Margin::ZERO).stroke(Stroke::new(1.0, p.popup_border));
        let response = egui::Modal::new(egui::Id::new("sql_scheduler")).frame(frame).show(ctx, |ui| {
            let (outer, _) = ui.allocate_exact_size(size, Sense::hover());
            let header = Rect::from_min_max(outer.min, pos2(outer.right(), outer.top() + 62.0));
            let service = Rect::from_min_max(pos2(outer.left(), header.bottom()), pos2(outer.right(), header.bottom() + 44.0));
            let footer = Rect::from_min_max(pos2(outer.left(), outer.bottom() - 82.0), outer.max);
            let body = Rect::from_min_max(pos2(outer.left(), service.bottom()), pos2(outer.right(), footer.top()));
            let line = Stroke::new(1.0, p.popup_border);
            for y in [header.bottom(), service.bottom(), footer.top()] {
                ui.painter().hline(outer.x_range(), y, line);
            }

            let mut heading = region(ui, header.shrink2(vec2(20.0, 9.0)), "scheduler_heading", Layout::top_down(Align::Min));
            heading.label(RichText::new(tr("Task scheduler")).font(theme::bold(19.0)));
            heading.label(RichText::new(tr("Schedule SQL queries and database backups")).small().color(p.muted));

            let mut status = region(ui, service.shrink2(vec2(20.0, 0.0)), "scheduler_service", Layout::right_to_left(Align::Center));
            if status
                .add_enabled(!busy, egui::Button::new(tr("Turn off")))
                .on_hover_text(tr("Pause all tasks and remove automatic startup; active tasks will finish"))
                .clicked()
            {
                *action = Some(Action::DisableService);
            }
            if !self.running
                && self.jobs.iter().any(|job| job.enabled)
                && status.add_enabled(!busy, egui::Button::new(tr("Enable background scheduler"))).clicked()
            {
                *action = Some(Action::EnableService);
            }
            status
                .add(
                    egui::Label::new(
                        RichText::new(if self.running { tr("Background scheduler is running") } else { tr("Background scheduler is not running") })
                            .color(p.text_muted),
                    )
                    .truncate(),
                )
                .on_hover_text(tr("Runs while you are signed in, including when DataCraft is closed."));
            status.colored_label(if self.running { p.ok } else { p.muted }, "●");

            let list_width = if size.x >= 740.0 { 230.0 } else { 0.0 };
            let detail = if list_width > 0.0 {
                let list = Rect::from_min_max(body.min, pos2(body.left() + list_width, body.bottom()));
                ui.painter().vline(list.right(), list.y_range(), line);
                let mut list_ui = region(ui, list.shrink2(vec2(12.0, 12.0)), "scheduler_tasks", Layout::top_down(Align::Min));
                if busy {
                    list_ui.disable();
                }
                if list_ui.add_sized([list_ui.available_width(), 32.0], egui::Button::new(tr("New task")).fill(p.selection)).clicked() {
                    self.pending_navigation = Some(Navigation::New);
                }
                list_ui.add_space(10.0);
                list_ui.label(RichText::new(tr("Tasks")).small().color(p.muted));
                egui::ScrollArea::vertical().id_salt("scheduler_task_list").auto_shrink([false, false]).show(&mut list_ui, |ui| {
                    if self.jobs.is_empty() {
                        ui.add_space(16.0);
                        ui.label(RichText::new(tr("No saved tasks")).color(p.muted));
                    }
                    for job in &self.jobs {
                        let interval = super::Interval::from_seconds(job.interval_seconds);
                        let subtitle = format!(
                            "{} · {} · {} {}",
                            if job.backup.is_some() { tr("Backup") } else { tr("SQL") },
                            if job.enabled { tr("Enabled") } else { tr("Paused") },
                            interval.value,
                            interval.unit.label()
                        );
                        let clicked = ui
                            .push_id(job.id, |ui| {
                                let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 62.0), Sense::click());
                                let selected = self.selected == Some(job.id);
                                let fill = if selected {
                                    p.selection
                                } else if response.hovered() {
                                    p.row_hover
                                } else {
                                    Color32::TRANSPARENT
                                };
                                ui.painter().rect_filled(rect, 6, fill);
                                let mut content = region(ui, rect.shrink2(vec2(12.0, 9.0)), "scheduler_task_card", Layout::top_down(Align::Min));
                                content.add(egui::Label::new(RichText::new(&job.name).strong()).truncate());
                                content.add(egui::Label::new(RichText::new(&subtitle).small().color(p.text_muted)).truncate());
                                response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), &job.name));
                                response.on_hover_text(&job.name).clicked()
                            })
                            .inner;
                        if clicked {
                            self.pending_navigation = Some(Navigation::Select(job.id));
                        }
                        ui.add_space(4.0);
                    }
                });
                Rect::from_min_max(pos2(list.right(), body.top()), body.max)
            } else {
                body
            };

            let mut details = region(ui, detail.shrink2(vec2(20.0, 12.0)), "scheduler_details", Layout::top_down(Align::Min));
            if list_width == 0.0 {
                details.add_enabled_ui(!busy, |ui| {
                    ui.horizontal(|ui| {
                        let picker_width = (ui.available_width() - 110.0).max(50.0);
                        ui.allocate_ui_with_layout(vec2(picker_width, 28.0), Layout::left_to_right(Align::Center), |ui| {
                            egui::ComboBox::from_id_salt("scheduler_task_picker")
                                .width(picker_width)
                                .truncate()
                                .selected_text(if self.draft.id == 0 { tr("New task").to_string() } else { self.draft.name.clone() })
                                .show_ui(ui, |ui| {
                                    for job in &self.jobs {
                                        if ui.selectable_label(self.selected == Some(job.id), &job.name).clicked() {
                                            self.pending_navigation = Some(Navigation::Select(job.id));
                                        }
                                    }
                                });
                        });
                        if ui.button(tr("New task")).clicked() {
                            self.pending_navigation = Some(Navigation::New);
                        }
                    });
                });
            }
            details.horizontal(|ui| {
                ui.heading(if self.draft.id == 0 { tr("New task") } else { tr("Task details") });
                if self.draft.id != 0 {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.add_enabled(!busy, egui::Button::new(tr("Delete task"))).clicked() {
                            self.confirm_delete = true;
                        }
                        if self.original.enabled && ui.add_enabled(!busy, egui::Button::new(tr("Pause task"))).clicked() {
                            *action = Some(Action::Pause(self.draft.id));
                        }
                    });
                }
            });
            details.add_space(6.0);
            details.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Configuration, tr("Configuration"));
                if ui.selectable_value(&mut self.tab, Tab::History, tr("History")).clicked() && !busy {
                    *action = Some(Action::Load);
                }
            });
            details.separator();
            match self.tab {
                Tab::Configuration => self.configuration(&mut details, connections, &p, busy),
                Tab::History => self.history(&mut details, &p, busy, action),
            }

            let mut footer_ui = region(ui, footer.shrink2(vec2(20.0, 8.0)), "scheduler_footer", Layout::top_down(Align::Min));
            let text = if busy {
                tr("Working…")
            } else if self.dirty() && !self.valid(connections) {
                if self.draft.backup.is_some() {
                    tr("Complete the task name, database, backup destination and interval.")
                } else {
                    tr("Complete the task name, data source, SQL and interval.")
                }
            } else if self.dirty() {
                tr("Unsaved changes")
            } else {
                ""
            };
            let (message, error) = self.message.as_ref().map_or((text, false), |(text, error)| (text.as_str(), *error));
            footer_ui
                .add_sized(
                    [footer_ui.available_width(), 20.0],
                    egui::Label::new(RichText::new(message).color(if error { p.error } else { p.muted })).truncate(),
                )
                .on_hover_text(message);
            footer_ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                if ui.add_enabled_ui(!busy && self.valid(connections) && self.dirty(), |ui| theme::primary_button(ui, tr("Save task"))).inner.clicked() {
                    self.open_confirmation(connections);
                }
                if theme::secondary_button(ui, tr("Cancel")).clicked() {
                    self.pending_navigation = Some(Navigation::Close);
                }
                if busy {
                    ui.spinner();
                }
            });
        });
        if !self.confirm_save && !self.confirm_delete && self.pending_navigation.is_none() {
            if !response.any_popup_open && ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
                self.pending_navigation = Some(Navigation::Close);
            }
            if !busy && self.valid(connections) && self.dirty() && ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::S)) {
                self.open_confirmation(connections);
            }
        }
    }

    fn configuration(&mut self, ui: &mut Ui, connections: &[ConnectionConfig], p: &Palette, busy: bool) {
        let height = ui.available_height().max(1.0);
        let editor_height = (height - 250.0).max(120.0);
        ui.add_enabled_ui(!busy, |ui| {
            egui::ScrollArea::vertical().id_salt("scheduler_configuration").max_height(height).auto_shrink([false, false]).show(ui, |ui| {
                let previous_backup = self.draft.backup.is_some();
                let mut backup_type = previous_backup;
                ui.horizontal(|ui| {
                    ui.label(tr("Task type"));
                    egui::ComboBox::from_id_salt("scheduled_task_type").selected_text(if backup_type { tr("Backup") } else { tr("SQL") }).show_ui(ui, |ui| {
                        ui.selectable_value(&mut backup_type, false, tr("SQL"));
                        ui.selectable_value(&mut backup_type, true, tr("Backup"));
                    });
                });
                if backup_type != previous_backup {
                    self.select_backup(backup_type, connections);
                    if backup_type && let Some(connection) = connections.iter().find(|c| c.id == self.draft.connection) {
                        self.load_catalog(ui.ctx(), connection, false);
                    }
                }
                ui.add_space(6.0);
                ui.label(tr("Name"));
                ui.add(egui::TextEdit::singleline(&mut self.draft.name).id_salt("task_name").desired_width(f32::INFINITY).hint_text(tr("Task name")));
                ui.add_space(6.0);
                let previous_source = self.draft.connection;
                ui.columns(2, |columns| {
                    columns[0].label(tr("Data source"));
                    let label = connections.iter().find(|c| c.id == self.draft.connection).map_or(tr("Select a data source"), |c| c.name.as_str());
                    egui::ComboBox::from_id_salt("scheduler_connection").width(columns[0].available_width()).truncate().selected_text(label).show_ui(
                        &mut columns[0],
                        |ui| {
                            for connection in connections
                                .iter()
                                .filter(|c| if backup_type { !crate::db::backup::BackupMode::for_driver(c.driver).is_empty() } else { !c.driver.is_nosql() })
                            {
                                ui.selectable_value(&mut self.draft.connection, connection.id, &connection.name);
                            }
                        },
                    );
                    columns[1].label(tr("Database"));
                    if backup_type {
                        if let Some(connection) = connections.iter().find(|c| c.id == self.draft.connection) {
                            self.database_picker(&mut columns[1], connection);
                        } else {
                            columns[1].label(tr("Select a data source"));
                        }
                    } else {
                        columns[1].add(egui::TextEdit::singleline(&mut self.draft.database).desired_width(f32::INFINITY).hint_text(tr("Default database")));
                    }
                });
                if previous_source != self.draft.connection {
                    self.source_changed(previous_source, connections);
                    if backup_type && let Some(connection) = connections.iter().find(|c| c.id == self.draft.connection) {
                        self.load_catalog(ui.ctx(), connection, false);
                    }
                }
                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(tr("Repeat every"));
                    ui.add(
                        egui::DragValue::new(&mut self.interval.value)
                            .range(1.0 / self.interval.unit.seconds() as f64..=31_536_000.0 / self.interval.unit.seconds() as f64)
                            .max_decimals(4)
                            .speed(1.0),
                    );
                    let mut unit = self.interval.unit;
                    egui::ComboBox::from_id_salt("scheduler_interval_unit").width(90.0).selected_text(unit.label()).show_ui(ui, |ui| {
                        for candidate in IntervalUnit::ALL {
                            ui.selectable_value(&mut unit, candidate, candidate.label());
                        }
                    });
                    if unit != self.interval.unit {
                        self.interval.change_unit(unit);
                    }
                    ui.add_space(8.0);
                    ui.checkbox(&mut self.draft.enabled, tr("Enabled"));
                });
                ui.add_space(8.0);
                if backup_type {
                    if let Some(connection) = connections.iter().find(|c| c.id == self.draft.connection) {
                        self.backup_configuration(ui, connection, p);
                    } else {
                        ui.label(tr("Select a data source that supports backups."));
                    }
                    return;
                }
                ui.horizontal(|ui| {
                    ui.label(RichText::new(tr("SQL")).strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if let Some(job) = self.jobs.iter().find(|job| job.id == self.draft.id && job.enabled) {
                            ui.label(RichText::new(format!("{}: {}", tr("Next run"), timestamp(job.next_run))).small().color(p.muted));
                        }
                    });
                });
                let driver = connections.iter().find(|c| c.id == self.draft.connection).map_or(Driver::Postgres, |c| c.driver);
                egui::Frame::NONE.fill(p.editor_bg).stroke(Stroke::new(1.0, p.control_border)).corner_radius(6).inner_margin(10).show(ui, |ui| {
                    let width = ui.available_width();
                    egui::ScrollArea::both().id_salt("scheduler_sql_scroll").max_height(editor_height).auto_shrink([false, false]).show(ui, |ui| {
                        let mut layouter = |ui: &Ui, text: &dyn egui::TextBuffer, _: f32| {
                            let mut job = crate::sql::highlight_for(text.as_str(), driver, p.dark, FontId::monospace(13.0));
                            job.wrap.max_width = f32::INFINITY;
                            ui.fonts_mut(|fonts| fonts.layout_job(job))
                        };
                        ui.add(
                            egui::TextEdit::multiline(&mut self.draft.sql)
                                .id_salt("scheduled_sql")
                                .code_editor()
                                .font(FontId::monospace(13.0))
                                .frame(egui::Frame::NONE)
                                .desired_width(width)
                                .min_size(vec2(width, editor_height - 4.0))
                                .layouter(&mut layouter),
                        );
                    });
                });
                ui.add_space(6.0);
                ui.label(RichText::new(tr("SQL is saved on this computer. Do not put passwords or tokens in SQL.")).small().color(p.muted));
            });
        });
    }

    fn history(&self, ui: &mut Ui, p: &Palette, busy: bool, action: &mut Option<Action>) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(tr("Recent runs")).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add_enabled(!busy && self.selected.is_some(), egui::Button::new(tr("Refresh history"))).clicked() {
                    *action = Some(Action::Load);
                }
            });
        });
        ui.add_space(8.0);
        egui::ScrollArea::vertical().id_salt("scheduler_history").auto_shrink([false, false]).show(ui, |ui| {
            if self.runs.is_empty() {
                ui.add_space(24.0);
                ui.label(RichText::new(tr("No runs yet")).color(p.muted));
            }
            for run in &self.runs {
                let (label, color): (&str, Color32) = match run.status.as_str() {
                    "running" => (tr("Running"), p.accent),
                    "succeeded" => (tr("Succeeded"), p.ok),
                    "failed" => (tr("Failed"), p.error),
                    "interrupted" => (tr("Interrupted"), p.error),
                    _ => (&run.status, p.muted),
                };
                egui::Frame::NONE.fill(p.control_bg).corner_radius(6).inner_margin(12).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(timestamp(run.started)).strong());
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.colored_label(color, label);
                        });
                    });
                    let duration = run.finished.map(|finished| format!(" · {} s", finished.saturating_sub(run.started))).unwrap_or_default();
                    ui.label(RichText::new(format!("#{}{duration}", run.id)).small().color(p.muted));
                    if !run.message.is_empty() {
                        ui.add(egui::Label::new(&run.message).wrap());
                    }
                });
                ui.add_space(6.0);
            }
        });
    }
}
