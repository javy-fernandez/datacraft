//! "Session Monitor…": a window (not modal) with the server's sessions of one connection, refreshed
//! on demand or on a timer, blocking chains highlighted, and Cancel Query / Terminate Session for
//! the selected session behind a confirmation.

use crate::db::Driver;
use crate::db::sessions::{self, Action, ActionKind, Highlight, SessionList, SessionRow};
use crate::i18n::{tr, trf};
use crate::ui::icons::Ico;
use crate::ui::theme::{self, Palette, palette};
use crate::worker::ConnId;
use egui::{Color32, FontId, Margin, RichText, Sense, Stroke, Ui, vec2};
use egui_extras::{Column, TableBuilder};
use std::time::{Duration, Instant};

/// Auto-refresh intervals in seconds (0 = off).
const INTERVALS: [u64; 4] = [0, 2, 5, 10];
/// The longest query text a tooltip shows.
const TOOLTIP_CHARS: usize = 2000;
/// Auto-refresh interval while DataCraft is not the focused window, in seconds.
const BACKGROUND_INTERVAL: u64 = 30;

/// How often to refresh: the chosen interval while focused, at most every 30 s in the
/// background, never while minimized or covered (no frames are drawn for nothing).
fn auto_interval(auto: u64, focused: bool, hidden: bool) -> Option<Duration> {
    match auto {
        0 => None,
        _ if hidden => None,
        _ if !focused => Some(Duration::from_secs(auto.max(BACKGROUND_INTERVAL))),
        every => Some(Duration::from_secs(every)),
    }
}

/// What the window asks the app to do.
#[derive(Debug, PartialEq, Eq)]
pub enum MonitorAction {
    Refresh,
    /// The action, and the epoch of the list its session was picked from.
    Act(Action, u64),
    Close,
}

pub struct SessionMonitor {
    pub conn: ConnId,
    pub driver: Driver,
    pub read_only: bool,
    /// Connection name.
    pub name: String,
    list: Option<SessionList>,
    /// The connection epoch the list was read in.
    epoch: u64,
    error: Option<String>,
    /// Local time of the last list.
    updated: Option<String>,
    last: Option<Instant>,
    /// Request id of the refresh under way, and of the action (with the action).
    pending: Option<u64>,
    acting: Option<(u64, Action)>,
    /// `SessionRow::label` of the selected row.
    selected: Option<String>,
    /// Auto-refresh interval in seconds (0 = off).
    auto: u64,
    hide_idle: bool,
    filter: String,
    /// Action waiting for confirmation, and the row as it was listed.
    confirm: Option<(Action, SessionRow)>,
    /// What the last action did (Ok) or why it failed.
    outcome: Option<Result<String, String>>,
}

impl SessionMonitor {
    pub fn new(conn: ConnId, driver: Driver, read_only: bool, name: String) -> Self {
        Self {
            conn,
            driver,
            read_only,
            name,
            list: None,
            epoch: 0,
            error: None,
            updated: None,
            last: None,
            pending: None,
            acting: None,
            selected: None,
            auto: 0,
            hide_idle: false,
            filter: String::new(),
            confirm: None,
            outcome: None,
        }
    }

    /// Follows the connection's current settings (edited data sources): another engine makes the
    /// list meaningless, read-only changes which actions are offered.
    pub fn sync(&mut self, driver: Driver, read_only: bool, name: &str) {
        if driver != self.driver {
            self.driver = driver;
            self.reset();
        }
        self.read_only = read_only;
        if self.name != name {
            self.name = name.to_string();
        }
    }

    /// A confirmation is open (keyboard shortcuts of the app are paused).
    pub fn confirming(&self) -> bool {
        self.confirm.is_some()
    }

    pub fn refresh_sent(&mut self, request_id: u64) {
        self.pending = Some(request_id);
        self.last = Some(Instant::now());
    }

    pub fn action_sent(&mut self, request_id: u64, action: Action) {
        self.acting = Some((request_id, action));
        self.outcome = None;
    }

    /// The connection was closed or opened again: what was listed (and picked from it) is gone,
    /// and a new list is read once it is connected.
    pub fn reset(&mut self) {
        *self = Self { auto: self.auto, hide_idle: self.hide_idle, filter: std::mem::take(&mut self.filter), ..Self::new(self.conn, self.driver, self.read_only, std::mem::take(&mut self.name)) };
    }

    /// A list reply; stale ones (an older refresh) are ignored.
    pub fn receive_list(&mut self, request_id: u64, epoch: u64, result: Result<SessionList, String>) {
        if self.pending != Some(request_id) {
            return;
        }
        self.pending = None;
        self.last = Some(Instant::now());
        match result {
            Ok(list) => {
                if !list.rows.iter().any(|r| Some(r.label()) == self.selected) {
                    self.selected = None;
                }
                self.list = Some(list);
                self.epoch = epoch;
                self.error = None;
                self.updated = Some(chrono::Local::now().format("%H:%M:%S").to_string());
            }
            Err(error) => self.error = Some(error),
        }
    }

    /// An action reply; the list is refreshed right after.
    pub fn receive_action(&mut self, request_id: u64, result: Result<(), String>) {
        let Some((_, action)) = self.acting.take_if(|(id, _)| *id == request_id) else { return };
        self.outcome = Some(result.map(|()| match action.kind {
            ActionKind::Cancel => trf("Query cancelled in session {id}", &[("id", &action.label())]),
            ActionKind::Terminate => trf("Session {id} terminated", &[("id", &action.label())]),
        }));
        self.last = None;
    }

    /// Documentation captures: a list as if just read, at a fixed time, with a row selected.
    #[cfg(all(test, target_os = "windows"))]
    pub(crate) fn show_list(&mut self, list: SessionList, updated: &str, selected: &str) {
        self.refresh_sent(0);
        self.receive_list(0, 1, Ok(list));
        self.updated = Some(updated.into());
        self.selected = Some(selected.into());
    }

    /// The selected row, while the filters show it (a hidden row is never acted on).
    fn selected_row(&self) -> Option<&SessionRow> {
        let selected = self.selected.as_ref()?;
        let rows = &self.list.as_ref()?.rows;
        let row = rows.iter().find(|r| r.label() == *selected)?;
        sessions::visible(row, &self.filter.trim().to_lowercase(), self.hide_idle, &sessions::blockers(rows)).then_some(row)
    }

    /// Why `kind` can't be done on the selected session (None when it can).
    fn disabled_reason(&self, kind: ActionKind, connected: bool) -> Option<&'static str> {
        let unsupported = match kind {
            ActionKind::Cancel => sessions::cancel_unsupported(self.driver),
            ActionKind::Terminate => sessions::terminate_unsupported(self.driver),
        };
        if unsupported.is_some() {
            return unsupported;
        }
        if self.read_only {
            return Some(tr("Read-only connection: sessions can be listed, not cancelled or terminated"));
        }
        if !connected {
            return Some(tr("The connection is not open"));
        }
        let Some(row) = self.selected_row() else { return Some(tr("Select a session")) };
        if row.own {
            return Some(tr("This is the monitor's own session"));
        }
        if row.restricted {
            return Some(tr("This session's details are hidden, so it can't be identified safely"));
        }
        if self.acting.is_some() {
            return Some(tr("Wait for the current action to finish"));
        }
        if kind == ActionKind::Cancel && !running(row) {
            return Some(tr("The session is not running a query"));
        }
        None
    }

    /// Draws the window; `connected` is the connection's state.
    pub fn show(&mut self, ctx: &egui::Context, connected: bool) -> Vec<MonitorAction> {
        let mut actions = Vec::new();
        let p = palette(ctx.global_style().visuals.dark_mode);
        let mut open = true;
        let frame = egui::Frame::window(&ctx.global_style()).fill(p.popup_bg).stroke(Stroke::new(1.0, p.popup_border)).corner_radius(12).inner_margin(Margin::same(14));
        let screen = ctx.content_rect();
        egui::Window::new(tr("Session Monitor"))
            .id(egui::Id::new(("session_monitor", self.conn)))
            .frame(frame)
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size(vec2(1180.0, 640.0).min(screen.size() - vec2(40.0, 40.0)))
            .min_size(vec2(880.0, 420.0))
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(screen.center())
            .show(ctx, |ui| self.contents(ui, &p, connected, &mut actions));
        self.confirmation(ctx, &p, &mut actions);
        if !open {
            actions.push(MonitorAction::Close);
        }
        // First list on open, then on the timer; never two at once.
        let (focused, hidden) = ctx.input(|i| (i.focused, i.viewport().minimized.unwrap_or(false) || i.viewport().occluded.unwrap_or(false)));
        if connected && self.pending.is_none() && self.acting.is_none() {
            match (self.last, auto_interval(self.auto, focused, hidden)) {
                (None, _) => actions.push(MonitorAction::Refresh),
                (Some(last), Some(every)) => match every.checked_sub(last.elapsed()) {
                    Some(left) if !left.is_zero() => ctx.request_repaint_after(left),
                    _ => actions.push(MonitorAction::Refresh),
                },
                _ => {}
            }
        }
        actions
    }

    fn contents(&mut self, ui: &mut Ui, p: &Palette, connected: bool, actions: &mut Vec<MonitorAction>) {
        ui.label(RichText::new(format!("{} · {}", self.name, self.driver.label())).color(p.muted));
        ui.add_space(6.0);
        self.toolbar(ui, p, connected, actions);
        ui.add_space(4.0);
        self.status(ui, p);
        ui.add_space(4.0);
        let detail_h = 168.0;
        let table_h = (ui.available_height() - detail_h - 10.0).max(90.0);
        let width = ui.available_width();
        ui.allocate_ui(vec2(width, table_h), |ui| {
            ui.set_min_size(vec2(width, table_h));
            self.table(ui, p, table_h);
        });
        ui.add_space(8.0);
        self.detail(ui, p);
    }

    fn toolbar(&mut self, ui: &mut Ui, p: &Palette, connected: bool, actions: &mut Vec<MonitorAction>) {
        let cancel_reason = self.disabled_reason(ActionKind::Cancel, connected);
        let terminate_reason = self.disabled_reason(ActionKind::Terminate, connected);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            {
                let refresh = ui.add_enabled_ui(connected && self.pending.is_none(), |ui| theme::tool_button(ui, Ico::Refresh, tr("Refresh"), false)).inner;
                if refresh.clicked() {
                    actions.push(MonitorAction::Refresh);
                }
                let background = tr("In the background, auto-refresh runs at most every 30 s, and it pauses while DataCraft is minimized");
                ui.label(tr("Auto-refresh")).on_hover_text(background);
                egui::ComboBox::from_id_salt(("session_auto", self.conn))
                    .width(64.0)
                    .selected_text(interval_label(self.auto))
                    .show_ui(ui, |ui| {
                        for every in INTERVALS {
                            ui.selectable_value(&mut self.auto, every, interval_label(every));
                        }
                    })
                    .response
                    .on_hover_text(background);
                if self.auto > 0 && !ui.input(|i| i.focused) {
                    ui.label(RichText::new(tr("every 30 s in the background")).color(p.muted)).on_hover_text(background);
                }
                ui.checkbox(&mut self.hide_idle, tr("Hide idle sessions"));
                ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(tr("Filter sessions")).desired_width(200.0));
                if self.pending.is_some() {
                    ui.add(egui::Spinner::new().size(14.0).color(p.muted));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                for (kind, label, reason) in [(ActionKind::Terminate, tr("Terminate Session…"), terminate_reason), (ActionKind::Cancel, tr("Cancel Query…"), cancel_reason)] {
                    let response = ui.add_enabled_ui(reason.is_none(), |ui| theme::secondary_button(ui, label)).inner;
                    if let Some(reason) = reason {
                        response.on_disabled_hover_text(reason);
                    } else if response.clicked()
                        && let Some(row) = self.selected_row()
                    {
                        self.confirm = Some((Action::new(kind, row), row.clone()));
                    }
                }
            });
        });
    }

    fn status(&self, ui: &mut Ui, p: &Palette) {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            match &self.list {
                Some(list) => {
                    let blockers = sessions::blockers(&list.rows);
                    let filter = self.filter.trim().to_lowercase();
                    let shown = list.rows.iter().filter(|r| sessions::visible(r, &filter, self.hide_idle, &blockers)).count();
                    ui.label(RichText::new(trf("{shown} of {total} sessions", &[("shown", &shown), ("total", &list.rows.len())])).color(p.text_muted));
                    let blocked = list.rows.iter().filter(|r| !r.blocked_by.is_empty()).count();
                    if blocked > 0 {
                        ui.label(RichText::new("·").color(p.muted));
                        swatch(ui, p.warn);
                        ui.label(RichText::new(trf("{n} blocked", &[("n", &blocked)])).color(p.warn));
                        swatch(ui, p.error);
                        ui.label(RichText::new(tr("Blocking others")).color(p.error));
                    }
                    if let Some(updated) = &self.updated {
                        ui.label(RichText::new("·").color(p.muted));
                        ui.label(RichText::new(trf("Updated {time}", &[("time", updated)])).color(p.muted));
                    }
                }
                None if self.error.is_none() => {
                    ui.label(RichText::new(tr("Loading sessions…")).color(p.muted));
                }
                None => {}
            }
        });
        if let Some(notice) = self.list.as_ref().and_then(|l| l.notice.as_ref()) {
            ui.add(egui::Label::new(RichText::new(notice).color(p.warn)).wrap());
        }
        if let Some(error) = &self.error {
            ui.add(egui::Label::new(RichText::new(error).color(p.error)).wrap());
        }
        match &self.outcome {
            Some(Ok(message)) => {
                ui.label(RichText::new(message).color(p.ok));
            }
            Some(Err(error)) => {
                ui.add(egui::Label::new(RichText::new(error).color(p.error)).wrap());
            }
            None if self.acting.is_some() => {
                ui.label(RichText::new(tr("Sending…")).color(p.muted));
            }
            None => {}
        }
    }

    fn table(&mut self, ui: &mut Ui, p: &Palette, height: f32) {
        // A new window's first (sizing) pass would keep the columns at their minimum widths.
        let Some(list) = self.list.as_ref().filter(|_| !ui.is_sizing_pass()) else { return };
        let blockers = sessions::blockers(&list.rows);
        let filter = self.filter.trim().to_lowercase();
        let rows: Vec<&SessionRow> = list.rows.iter().filter(|r| sessions::visible(r, &filter, self.hide_idle, &blockers)).collect();
        let headers = [tr("Session"), tr("User"), tr("Database"), tr("Client"), tr("Application"), tr("State"), tr("Duration"), tr("Wait"), tr("Blocked by"), tr("Query")];
        let mut clicked = None;
        let area = ui.max_rect();
        // No gaps between cells, so a highlighted row is one band.
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        let row_h = 24.0;
        TableBuilder::new(ui)
            .id_salt(("session_table", self.conn))
            .resizable(true)
            .striped(false)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .sense(Sense::click())
            .auto_shrink([false, false])
            .min_scrolled_height(0.0)
            .max_scroll_height((height - row_h).max(row_h))
            .column(Column::initial(118.0).at_least(70.0).clip(true))
            .column(Column::initial(84.0).at_least(40.0).clip(true))
            .column(Column::initial(84.0).at_least(40.0).clip(true))
            .column(Column::initial(140.0).at_least(40.0).clip(true))
            .column(Column::initial(100.0).at_least(40.0).clip(true))
            .column(Column::initial(132.0).at_least(40.0).clip(true))
            .column(Column::initial(80.0).at_least(40.0).clip(true))
            .column(Column::initial(140.0).at_least(40.0).clip(true))
            .column(Column::initial(76.0).at_least(40.0).clip(true))
            .column(Column::remainder().at_least(120.0).clip(true))
            .header(row_h, |mut header| {
                for label in headers {
                    header.col(|ui| {
                        ui.painter().rect_filled(ui.max_rect(), 0.0, p.header_bg);
                        ui.add_space(8.0);
                        ui.add(egui::Label::new(RichText::new(label).strong().color(p.text_muted)).truncate());
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, rows.len(), |mut row| {
                    let session = rows[row.index()];
                    let label = session.label();
                    let selected = self.selected.as_deref() == Some(label.as_str());
                    let mark = sessions::highlight(session, &blockers);
                    let tint = match mark {
                        Highlight::Blocked => Some(blocked_color(p)),
                        Highlight::Blocker => Some(blocker_color(p)),
                        Highlight::None => None,
                    };
                    row.set_selected(selected);
                    let cell = |ui: &mut Ui| {
                        if let Some(tint) = tint.filter(|_| !selected) {
                            ui.painter().rect_filled(ui.max_rect(), 0.0, tint);
                        }
                        ui.add_space(8.0);
                    };
                    row.col(|ui| {
                        cell(ui);
                        if let Some(color) = match mark {
                            Highlight::Blocked => Some(p.warn),
                            Highlight::Blocker => Some(p.error),
                            Highlight::None => None,
                        } {
                            let r = ui.max_rect();
                            ui.painter().rect_filled(egui::Rect::from_min_size(r.min, vec2(3.0, r.height())), 0.0, color);
                        }
                        let text = RichText::new(&label).monospace().size(12.0).color(p.text);
                        let response = ui.add(egui::Label::new(if session.own { text.italics() } else { text }).truncate().selectable(false));
                        let badge = if session.own { Some(tr("monitor")) } else if session.datacraft { Some("DataCraft") } else { None };
                        if let Some(badge) = badge {
                            let at = response.rect.right_center() + vec2(6.0, 0.0);
                            theme::paint_badge(ui, at, badge);
                        }
                    });
                    for value in [&session.user, &session.database, &session.client, &session.application] {
                        row.col(|ui| {
                            cell(ui);
                            ui.add(egui::Label::new(RichText::new(value).color(p.text)).truncate().selectable(false));
                        });
                    }
                    row.col(|ui| {
                        cell(ui);
                        let mut state = if session.restricted { tr("insufficient privilege").to_string() } else { session.state.clone() };
                        if session.in_transaction {
                            state = format!("{state} · {}", tr("in transaction"));
                        }
                        let color = if running(session) { p.text } else { p.text_muted };
                        ui.add(egui::Label::new(RichText::new(state).color(color)).truncate().selectable(false));
                    });
                    row.col(|ui| {
                        cell(ui);
                        let text = session.duration_ms.map(sessions::format_duration).unwrap_or_default();
                        ui.add(egui::Label::new(RichText::new(text).color(p.text)).truncate().selectable(false));
                    });
                    row.col(|ui| {
                        cell(ui);
                        let color = if mark == Highlight::Blocked { p.warn } else { p.text_muted };
                        ui.add(egui::Label::new(RichText::new(&session.wait).color(color)).truncate().selectable(false));
                    });
                    row.col(|ui| {
                        cell(ui);
                        ui.add(egui::Label::new(RichText::new(session.blocked_by.join(", ")).monospace().size(12.0).color(p.warn)).truncate().selectable(false));
                    });
                    row.col(|ui| {
                        cell(ui);
                        let line = session.query.split_whitespace().collect::<Vec<_>>().join(" ");
                        let response = ui.add(egui::Label::new(RichText::new(line).monospace().size(12.0).color(p.text)).truncate().selectable(false));
                        if !session.query.trim().is_empty() {
                            response.on_hover_ui(|ui| {
                                ui.set_max_width(640.0);
                                ui.add(egui::Label::new(RichText::new(preview(&session.query, TOOLTIP_CHARS)).monospace().size(12.0)).wrap());
                            });
                        }
                    });
                    let response = row.response();
                    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, trf("Session {id}", &[("id", &label)])));
                    if response.clicked() || response.secondary_clicked() {
                        clicked = Some(label.clone());
                    }
                    response.context_menu(|ui| {
                        if ui.button(tr("Copy Query")).clicked() {
                            ui.ctx().copy_text(session.query.clone());
                            ui.close();
                        }
                        if ui.button(tr("Copy Session ID")).clicked() {
                            ui.ctx().copy_text(label.clone());
                            ui.close();
                        }
                    });
                });
            });
        if let Some(label) = clicked {
            self.selected = Some(label);
        }
        if rows.is_empty() {
            ui.painter().text(area.center(), egui::Align2::CENTER_CENTER, tr("No matching sessions"), FontId::proportional(13.0), p.muted);
        }
    }

    fn detail(&self, ui: &mut Ui, p: &Palette) {
        let dark = p.dark;
        egui::Frame::NONE.fill(p.editor_bg).stroke(Stroke::new(1.0, p.control_border)).corner_radius(6).inner_margin(Margin::same(8)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(ui.available_height().max(60.0));
            let Some(row) = self.selected_row() else {
                ui.label(RichText::new(tr("Select a session to see its query")).color(p.muted));
                return;
            };
            ui.horizontal(|ui| {
                ui.label(RichText::new(trf("Session {id}", &[("id", &row.label())])).font(theme::bold(13.0)).color(p.text));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add_enabled(!row.query.is_empty(), egui::Button::new(tr("Copy Query")).small()).clicked() {
                        ui.ctx().copy_text(row.query.clone());
                    }
                });
            });
            {
                {
                    let mut parts = vec![format!("{}@{}", row.user, if row.client.is_empty() { "?" } else { &row.client })];
                    parts.extend([&row.database, &row.application, &row.state].into_iter().filter(|v| !v.is_empty()).cloned());
                    if let Some(ms) = row.duration_ms {
                        parts.push(sessions::format_duration(ms));
                    }
                    if !row.blocked_by.is_empty() {
                        parts.push(trf("blocked by {ids}", &[("ids", &row.blocked_by.join(", "))]));
                    }
                    ui.add(egui::Label::new(RichText::new(parts.join(" · ")).color(p.muted)).truncate());
                }
            }
            ui.add_space(4.0);
            egui::ScrollArea::vertical().id_salt(("session_query", self.conn)).auto_shrink([false, false]).show(ui, |ui| {
                if row.query.trim().is_empty() {
                    ui.label(RichText::new(tr("No query text")).color(p.muted));
                } else {
                    let mut job = crate::sql::highlight(&row.query, dark, FontId::monospace(12.5));
                    job.wrap.max_width = ui.available_width();
                    ui.add(egui::Label::new(job).selectable(true));
                }
            });
        });
    }

    fn confirmation(&mut self, ctx: &egui::Context, p: &Palette, actions: &mut Vec<MonitorAction>) {
        let Some((action, row)) = &self.confirm else { return };
        let dark = p.dark;
        let mut close = false;
        let frame = egui::Frame::popup(&ctx.global_style()).fill(p.popup_bg).corner_radius(12).inner_margin(Margin::same(20)).stroke(Stroke::new(1.0, p.popup_border));
        let (title, question, button) = match action.kind {
            ActionKind::Cancel => (tr("Cancel Query"), trf("Cancel the query running in session {id}?", &[("id", &action.label())]), tr("Cancel Query")),
            ActionKind::Terminate => (tr("Terminate Session"), trf("Terminate session {id}?", &[("id", &action.label())]), tr("Terminate Session")),
        };
        let sql = sessions::action_sql(self.driver, action);
        egui::Modal::new(egui::Id::new(("session_monitor_confirm", self.conn))).frame(frame).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.label(RichText::new(title).font(theme::bold(15.0)).color(p.text));
            ui.add_space(8.0);
            ui.label(question);
            let who = [&row.user, &row.database, &row.client, &row.application].into_iter().filter(|v| !v.is_empty()).cloned().collect::<Vec<_>>().join(" · ");
            if !who.is_empty() {
                ui.label(RichText::new(who).color(p.muted));
            }
            ui.add_space(4.0);
            let warning = match action.kind {
                ActionKind::Cancel => tr("The statement stops with an error; the session and its transaction stay open."),
                ActionKind::Terminate => tr("The client is disconnected and its open transaction is rolled back."),
            };
            ui.add(egui::Label::new(RichText::new(warning).color(p.error)).wrap());
            if !row.query.trim().is_empty() {
                ui.add_space(8.0);
                ui.label(RichText::new(tr("Query")).strong());
                egui::Frame::NONE.fill(p.editor_bg).stroke(Stroke::new(1.0, p.control_border)).corner_radius(6).inner_margin(Margin::same(8)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    egui::ScrollArea::vertical().max_height(90.0).show(ui, |ui| {
                        let mut job = crate::sql::highlight(&preview(&row.query, 600), dark, FontId::monospace(12.0));
                        job.wrap.max_width = ui.available_width();
                        ui.label(job);
                    });
                });
            }
            ui.add_space(8.0);
            ui.label(RichText::new(tr("Statement")).strong());
            egui::Frame::NONE.fill(p.editor_bg).stroke(Stroke::new(1.0, p.control_border)).corner_radius(6).inner_margin(Margin::same(8)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                match &sql {
                    Ok(sql) => {
                        let mut job = crate::sql::highlight(sql, dark, FontId::monospace(12.0));
                        job.wrap.max_width = ui.available_width();
                        ui.label(job);
                    }
                    Err(error) => {
                        ui.add(egui::Label::new(RichText::new(format!("{error:#}")).color(p.error)).wrap());
                    }
                }
            });
            ui.add_space(16.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let danger = egui::Button::new(RichText::new(button).color(Color32::WHITE)).fill(p.stop).stroke(Stroke::new(1.0, p.stop)).corner_radius(6).min_size(vec2(72.0, 28.0));
                if ui.add_enabled(sql.is_ok(), danger).clicked() {
                    actions.push(MonitorAction::Act(action.clone(), self.epoch));
                    close = true;
                }
                if theme::secondary_button(ui, tr("Go Back")).clicked() {
                    close = true;
                }
            });
        });
        if close || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.confirm = None;
        }
    }
}

/// Running a statement (not idle, nor idle in a transaction).
fn running(row: &SessionRow) -> bool {
    !row.idle && !row.in_transaction && !row.state.to_ascii_lowercase().starts_with("idle")
}

fn interval_label(every: u64) -> String {
    if every == 0 { tr("Off").to_string() } else { trf("{n} s", &[("n", &every)]) }
}

fn blocked_color(p: &Palette) -> Color32 {
    theme::blend(p.popup_bg, p.warn.gamma_multiply(if p.dark { 0.22 } else { 0.16 }))
}

fn blocker_color(p: &Palette) -> Color32 {
    theme::blend(p.popup_bg, p.error.gamma_multiply(if p.dark { 0.24 } else { 0.14 }))
}

/// Small square of a legend color.
fn swatch(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
    ui.painter().rect_filled(rect, 2.0, color);
}

/// The start of a text that can be very long.
fn preview(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, state: &str, own: bool) -> SessionRow {
        SessionRow { id: id.into(), state: state.into(), own, idle: state == "idle", ..Default::default() }
    }

    fn monitor(driver: Driver, read_only: bool) -> SessionMonitor {
        let mut m = SessionMonitor::new(1, driver, read_only, "local".into());
        m.refresh_sent(7);
        let mut hidden = session("13", "", false);
        hidden.restricted = true;
        m.receive_list(7, 3, Ok(SessionList { rows: vec![session("10", "active", false), session("11", "idle", false), session("12", "active", true), hidden], notice: None }));
        m
    }

    #[test]
    fn actions_are_offered_only_where_they_can_run() {
        let mut m = monitor(Driver::Postgres, false);
        assert_eq!(m.disabled_reason(ActionKind::Cancel, true), Some(tr("Select a session")));
        m.selected = Some("10".into());
        assert_eq!((m.disabled_reason(ActionKind::Cancel, true), m.disabled_reason(ActionKind::Terminate, true)), (None, None));
        assert!(m.disabled_reason(ActionKind::Cancel, false).is_some());
        m.selected = Some("11".into());
        assert_eq!(m.disabled_reason(ActionKind::Cancel, true), Some(tr("The session is not running a query")));
        assert_eq!(m.disabled_reason(ActionKind::Terminate, true), None);
        m.selected = Some("12".into());
        assert_eq!(m.disabled_reason(ActionKind::Terminate, true), Some(tr("This is the monitor's own session")));
        m.selected = Some("13".into());
        assert_eq!(m.disabled_reason(ActionKind::Terminate, true), Some(tr("This session's details are hidden, so it can't be identified safely")));

        let mut ro = monitor(Driver::Postgres, true);
        ro.selected = Some("10".into());
        assert!(ro.disabled_reason(ActionKind::Terminate, true).is_some() && ro.disabled_reason(ActionKind::Cancel, true).is_some());
        let mut ms = monitor(Driver::MsSql, false);
        ms.selected = Some("10".into());
        assert_eq!(ms.disabled_reason(ActionKind::Cancel, true), sessions::cancel_unsupported(Driver::MsSql));
        assert_eq!(ms.disabled_reason(ActionKind::Terminate, true), None);
    }

    #[test]
    fn auto_refresh_slows_in_the_background_and_pauses_when_hidden() {
        let s = Duration::from_secs;
        assert_eq!(auto_interval(0, true, false), None);
        assert_eq!(auto_interval(2, true, false), Some(s(2)));
        assert_eq!(auto_interval(2, false, false), Some(s(30)));
        assert_eq!(auto_interval(10, false, false), Some(s(30)));
        assert_eq!(auto_interval(5, true, true), None);
        assert_eq!(auto_interval(5, false, true), None);
    }

    #[test]
    fn follows_the_edited_data_source() {
        let mut m = monitor(Driver::Postgres, false);
        m.selected = Some("10".into());
        m.sync(Driver::Postgres, true, "renamed");
        assert!(m.list.is_some() && m.name == "renamed");
        assert!(m.disabled_reason(ActionKind::Terminate, true).is_some(), "now read-only");
        m.sync(Driver::MySql, true, "renamed");
        assert!(m.list.is_none() && m.selected.is_none(), "another engine: the old list is dropped");
        assert_eq!(m.driver, Driver::MySql);
    }

    #[test]
    fn hidden_rows_are_never_acted_on() {
        let mut m = monitor(Driver::Postgres, false);
        m.selected = Some("11".into());
        assert_eq!(m.disabled_reason(ActionKind::Terminate, true), None);
        m.hide_idle = true;
        assert_eq!(m.disabled_reason(ActionKind::Terminate, true), Some(tr("Select a session")), "hidden by Hide idle sessions");
        m.hide_idle = false;
        m.filter = "no such session".into();
        assert!(m.selected_row().is_none(), "hidden by the filter");
        m.filter.clear();
        assert_eq!(m.selected_row().map(|r| r.id.as_str()), Some("11"), "shown again, selected again");
    }

    #[test]
    fn a_reconnect_clears_the_list_and_actions_carry_its_epoch() {
        let mut m = monitor(Driver::Postgres, false);
        assert_eq!(m.epoch, 3);
        m.selected = Some("10".into());
        m.confirm = Some((Action::new(ActionKind::Cancel, &session("10", "active", false)), session("10", "active", false)));
        m.hide_idle = true;
        m.refresh_sent(9);
        m.reset();
        assert!(m.list.is_none() && m.selected.is_none() && m.confirm.is_none() && m.pending.is_none() && m.last.is_none());
        assert!(m.hide_idle, "the view settings stay");
        m.receive_list(9, 3, Ok(SessionList::default()));
        assert!(m.list.is_none(), "a reply to a request from before the reset is ignored");
    }

    #[test]
    fn stale_replies_are_ignored_and_actions_refresh() {
        let mut m = monitor(Driver::Postgres, false);
        m.refresh_sent(8);
        m.receive_list(7, 3, Err("old".into()));
        assert!(m.error.is_none() && m.pending == Some(8));
        m.receive_list(8, 3, Err("permission denied for pg_stat_activity".into()));
        assert_eq!(m.error.as_deref(), Some("permission denied for pg_stat_activity"));
        assert_eq!(m.list.as_ref().map(|l| l.rows.len()), Some(4), "the last list stays");
        let action = Action::new(ActionKind::Terminate, &session("10", "active", false));
        m.action_sent(9, action);
        m.receive_action(3, Ok(()));
        assert!(m.acting.is_some());
        m.receive_action(9, Ok(()));
        assert!(m.acting.is_none() && m.last.is_none(), "a refresh follows");
        assert_eq!(m.outcome, Some(Ok("Session 10 terminated".into())));
    }
}
