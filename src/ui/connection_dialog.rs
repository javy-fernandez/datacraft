//! DataGrip's "Data Sources and Drivers" window: the project's data sources on the left and the
//! selected one's settings on the right (General / Options / SSH / Advanced tabs), with Test
//! Connection, Cancel, Apply and OK at the bottom.

use crate::db::datagrip;
use crate::db::url::{apply_url, jdbc_url};
use crate::db::ssh::{self, SshAuth};
use crate::db::tls::{self, TlsMode};
use crate::db::{AuthMode, ConnectionConfig, Driver, Environment, is_secret_property, snowflake};
use crate::i18n::{tr, trf};
use crate::ui::icons::{self, Ico};
use crate::ui::theme::{self, Palette, palette};
use egui::{Align, Layout, Rect, RichText, Sense, Stroke, Ui, UiBuilder, pos2, vec2};

pub enum TestState {
    Idle,
    Testing,
    /// `Ok` carries the server's product and version when it could be read.
    Done(Result<Option<String>, String>),
}

pub enum DialogResult {
    /// Apply (`close` false) or OK: data sources to save and ids to delete.
    Save { save: Vec<ConnectionConfig>, delete: Vec<u64>, close: bool },
    Test(Box<ConnectionConfig>),
    Cancel,
}

/// One data source of the list, as edited.
struct Item {
    cfg: ConnectionConfig,
    /// As last saved; None for a data source that doesn't exist yet.
    saved: Option<ConnectionConfig>,
}

impl Item {
    fn dirty(&self) -> bool {
        self.saved.as_ref() != Some(&self.cfg)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    General,
    Options,
    /// The SSH tunnel, for the engines that can go through one.
    Ssh,
    Advanced,
}

pub struct ConnectionDialog {
    items: Vec<Item>,
    selected: usize,
    /// Saved data sources removed with "−", deleted on Apply/OK.
    removed: Vec<u64>,
    /// Test Connection of the data source with this id.
    test: (u64, TestState),
    error: Option<String>,
    tab: Tab,
    /// Text of the URL field. Rebuilt from the fields every frame unless it is being edited, when
    /// it is parsed back into them instead.
    url: String,
    /// Cancel / Escape with unapplied changes: asking before they are discarded.
    confirm_discard: bool,
    /// The bastion's key was just trusted: Test Connection runs again.
    retest: bool,
    /// "Import from DataGrip", open.
    import: Option<Import>,
}

/// "Import from DataGrip": the files found, the one read and its data sources, ticked or not.
struct Import {
    files: Vec<std::path::PathBuf>,
    file: Option<std::path::PathBuf>,
    found: Vec<(datagrip::Found, bool)>,
    error: Option<String>,
}

const LIST_WIDTH: f32 = 290.0;
const LABEL_WIDTH: f32 = 118.0;
const TITLE_H: f32 = 40.0;
const FOOTER_H: f32 = 56.0;
const TEST_H: f32 = 40.0;

impl ConnectionDialog {
    /// The window with the existing data sources and a new one of `id`, selected.
    pub fn new_connection(id: u64, existing: &[ConnectionConfig]) -> Self {
        let mut d = Self::with(existing);
        d.add(id, Driver::Postgres);
        d
    }

    /// The window with `id` selected.
    pub fn edit(id: u64, existing: &[ConnectionConfig]) -> Self {
        let mut d = Self::with(existing);
        d.selected = d.items.iter().position(|i| i.cfg.id == id).unwrap_or(0);
        d
    }

    fn with(existing: &[ConnectionConfig]) -> Self {
        let items = existing.iter().map(|c| Item { cfg: c.clone(), saved: Some(c.clone()) }).collect();
        Self { items, selected: 0, removed: Vec::new(), test: (0, TestState::Idle), error: None, tab: Tab::General, url: String::new(), confirm_discard: false, retest: false, import: None }
    }

    fn add(&mut self, id: u64, driver: Driver) {
        let mut cfg = ConnectionConfig::new(id, driver);
        // The id is a timestamp: it is not shown in the default name.
        cfg.name = driver.label().to_string();
        self.items.push(Item { cfg, saved: None });
        self.select(self.items.len() - 1);
    }

    fn select(&mut self, i: usize) {
        self.selected = i;
        self.error = None;
        if self.items.get(i).is_none_or(|it| !Self::has_tab(it.cfg.driver, self.tab)) {
            self.tab = Tab::General;
        }
    }

    /// Advanced is Snowflake's; SSH is for the engines a tunnel can reach.
    fn has_tab(driver: Driver, tab: Tab) -> bool {
        match tab {
            Tab::General | Tab::Options => true,
            Tab::Ssh => driver.supports_ssh() || tls::supported(driver),
            Tab::Advanced => driver == Driver::Snowflake,
        }
    }

    pub fn set_test_result(&mut self, id: u64, result: Result<Option<String>, String>) {
        if self.test.0 == id {
            self.test.1 = TestState::Done(result);
        }
    }

    /// An id not used by any data source of the list (they are millisecond timestamps).
    fn next_id(&self) -> u64 {
        let millis = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        let max = self.items.iter().map(|i| i.cfg.id).chain(self.removed.iter().copied()).max().unwrap_or(0);
        (max + 1).max(millis)
    }

    fn validate(cfg: &ConnectionConfig) -> Result<(), String> {
        if cfg.name.trim().is_empty() {
            return Err(tr("Name is required").into());
        }
        match cfg.driver {
            Driver::Sqlite if cfg.file_path.trim().is_empty() => Err(tr("Specify the database file").into()),
            Driver::Sqlite => Ok(()),
            _ if cfg.host.trim().is_empty() => Err(tr("Host is required").into()),
            Driver::Snowflake => snowflake::credentials(cfg).map(|_| ()).map_err(|e| e.to_string()),
            _ if tls::supported(cfg.driver) && cfg.tls.mode != TlsMode::Disable && cfg.tls.cert_file.trim().is_empty() != cfg.tls.key_file.trim().is_empty() => {
                Err(tr("The client certificate and the client key go together").into())
            }
            _ if ssh::tunnels(cfg) => match &cfg.ssh {
                s if s.host.trim().is_empty() => Err(tr("The SSH host is required").into()),
                s if s.user.trim().is_empty() => Err(tr("The SSH user is required").into()),
                s if s.auth == SshAuth::KeyPair && s.key_file.trim().is_empty() => Err(tr("Choose the SSH private key file").into()),
                _ => Ok(()),
            },
            _ => Ok(()),
        }
    }

    /// The config to save or test: property rows without a name are dropped.
    fn finished(cfg: &ConnectionConfig) -> ConnectionConfig {
        let mut cfg = cfg.clone();
        cfg.ask_password &= cfg.password.is_empty();
        cfg.properties.retain(|(k, _)| !k.trim().is_empty());
        for (k, _) in &mut cfg.properties {
            *k = k.trim().to_string();
        }
        cfg
    }

    /// Apply / OK: every changed data source must be valid; the first that isn't is selected.
    fn save(&mut self, close: bool) -> Option<DialogResult> {
        for i in 0..self.items.len() {
            if self.items[i].dirty()
                && let Err(e) = Self::validate(&self.items[i].cfg)
            {
                self.select(i);
                self.error = Some(e);
                return None;
            }
        }
        let mut save = Vec::new();
        for item in self.items.iter_mut().filter(|i| i.dirty()) {
            item.cfg = Self::finished(&item.cfg);
            item.saved = Some(item.cfg.clone());
            save.push(item.cfg.clone());
        }
        self.error = None;
        Some(DialogResult::Save { save, delete: std::mem::take(&mut self.removed), close })
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Option<DialogResult> {
        let mut result = None;
        let p = palette(ctx.global_style().visuals.dark_mode);
        let frame = egui::Frame::popup(&ctx.global_style())
            .fill(p.popup_bg)
            .corner_radius(12)
            .inner_margin(egui::Margin::ZERO)
            .stroke(Stroke::new(1.0, p.popup_border));
        let screen = ctx.content_rect();
        let size = vec2((screen.width() - 80.0).clamp(640.0, 1000.0), (screen.height() - 60.0).clamp(480.0, 760.0));
        egui::Modal::new(egui::Id::new("connection_dialog")).frame(frame).show(ctx, |ui| {
            let (outer, _) = ui.allocate_exact_size(size, Sense::hover());
            let line = Stroke::new(1.0, p.popup_border);
            let body = Rect::from_min_max(pos2(outer.left(), outer.top() + TITLE_H), pos2(outer.right(), outer.bottom() - FOOTER_H));
            let list = Rect::from_min_max(body.min, pos2(body.left() + LIST_WIDTH, body.bottom()));
            let right = Rect::from_min_max(pos2(list.right() + 1.0, body.top()), body.max);

            ui.painter().text(
                pos2(outer.left() + 18.0, outer.top() + TITLE_H / 2.0),
                egui::Align2::LEFT_CENTER,
                tr("Data Sources and Drivers"),
                theme::bold(13.0),
                p.text,
            );
            ui.painter().hline(outer.x_range(), body.top(), line);
            ui.painter().vline(list.right() + 0.5, body.y_range(), line);
            ui.painter().hline(outer.x_range(), body.bottom(), line);

            let mut list_ui = ui.new_child(UiBuilder::new().max_rect(list.shrink2(vec2(10.0, 10.0))));
            self.data_source_list(&mut list_ui, &p);

            let test_rect = Rect::from_min_max(pos2(right.left(), right.bottom() - TEST_H), right.max);
            ui.painter().hline(right.x_range(), test_rect.top(), line);
            let form = Rect::from_min_max(right.min, pos2(right.right(), test_rect.top())).shrink2(vec2(18.0, 14.0));
            let mut form_ui = ui.new_child(UiBuilder::new().max_rect(form));
            if !self.items.is_empty() {
                self.details(&mut form_ui, &p);
            }

            let mut test_ui = ui.new_child(
                UiBuilder::new().max_rect(test_rect.shrink2(vec2(18.0, 0.0))).layout(Layout::left_to_right(Align::Center)),
            );
            if let Some(r) = self.test_row(&mut test_ui, &p) {
                result = Some(r);
            }

            let footer = Rect::from_min_max(pos2(outer.left(), body.bottom()), outer.max).shrink2(vec2(18.0, 0.0));
            let mut footer_ui = ui.new_child(UiBuilder::new().max_rect(footer).layout(Layout::right_to_left(Align::Center)));
            footer_ui.spacing_mut().item_spacing.x = 10.0;
            if theme::primary_button(&mut footer_ui, tr("OK")).clicked()
                && let Some(r) = self.save(true)
            {
                result = Some(r);
            }
            let dirty = !self.removed.is_empty() || self.items.iter().any(Item::dirty);
            if footer_ui.add_enabled(dirty, egui::Button::new(tr("Apply")).corner_radius(6).min_size(vec2(72.0, 28.0))).clicked()
                && let Some(r) = self.save(false)
            {
                result = Some(r);
            }
            if theme::secondary_button(&mut footer_ui, tr("Cancel")).clicked() {
                result = self.cancel(dirty);
            }
            if let Some(e) = &self.error {
                footer_ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.add(egui::Label::new(RichText::new(e).color(p.error)).truncate()).on_hover_text(e);
                });
            }
        });
        let escape = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        if self.import.is_some() {
            if self.import_window(ctx, &p) || escape {
                self.import = None;
            }
            return None;
        }
        if self.confirm_discard {
            let changed = self.items.iter().filter(|i| i.dirty()).count() + self.removed.len();
            let mut decision = None;
            egui::Modal::new(egui::Id::new("confirm_discard_data_sources")).frame(confirm_frame(ctx, &p)).show(ctx, |ui| {
                ui.set_width(380.0);
                ui.label(RichText::new(tr("Unsaved changes")).font(theme::bold(15.0)));
                ui.add_space(8.0);
                ui.label(trf("Changes to {count} data source(s) have not been applied. Discard them?", &[("count", &changed)]));
                ui.add_space(16.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if theme::primary_button(ui, tr("Discard")).clicked() {
                        decision = Some(true);
                    }
                    if theme::secondary_button(ui, tr("Cancel")).clicked() {
                        decision = Some(false);
                    }
                });
            });
            // Escape here backs out of the question, not out of the window.
            match decision.or(escape.then_some(false)) {
                Some(true) => return Some(DialogResult::Cancel),
                Some(false) => self.confirm_discard = false,
                None => {}
            }
            return None;
        }
        if result.is_none() && escape {
            let dirty = !self.removed.is_empty() || self.items.iter().any(Item::dirty);
            result = self.cancel(dirty);
        }
        result
    }

    /// Cancel closes right away when nothing changed; otherwise it asks first.
    fn cancel(&mut self, dirty: bool) -> Option<DialogResult> {
        if dirty {
            self.confirm_discard = true;
            None
        } else {
            Some(DialogResult::Cancel)
        }
    }

    /// Left pane: "Data Sources", the +/− toolbar and the project's data sources.
    fn data_source_list(&mut self, ui: &mut Ui, p: &Palette) {
        ui.label(RichText::new(tr("Data Sources")).font(theme::bold(13.0)).color(p.text));
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let plus = theme::icon_button(ui, Ico::Plus, true, tr("Add data source"));
            let mut add = None;
            egui::Popup::menu(&plus).show(|ui| {
                ui.set_min_width(180.0);
                for d in Driver::ALL {
                    let r = ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
                        icons::paint(ui.painter(), r, Ico::Driver(d), p.icon, p, true);
                        ui.add(egui::Button::new(d.label()).frame(false))
                    });
                    if r.inner.clicked() {
                        add = Some(d);
                    }
                }
                ui.separator();
                if ui.button(tr("Import from DataGrip…")).clicked() {
                    self.open_import(datagrip::default_files());
                }
            });
            if let Some(d) = add {
                let id = self.next_id();
                self.add(id, d);
            }
            if theme::icon_button(ui, Ico::Minus, !self.items.is_empty(), tr("Remove")).clicked() {
                let item = self.items.remove(self.selected);
                if item.saved.is_some() {
                    self.removed.push(item.cfg.id);
                }
                let next = self.selected.min(self.items.len().saturating_sub(1));
                self.select(next);
            }
            if theme::icon_button(ui, Ico::Copy, !self.items.is_empty(), tr("Duplicate")).clicked() {
                let mut cfg = self.items[self.selected].cfg.clone();
                cfg.id = self.next_id();
                cfg.name = trf("{name} (copy)", &[("name", &cfg.name)]);
                self.items.push(Item { cfg, saved: None });
                self.select(self.items.len() - 1);
            }
        });
        ui.add_space(8.0);
        ui.label(RichText::new(tr("Project Data Sources")).color(p.muted));
        ui.add_space(2.0);
        let mut clicked = None;
        egui::ScrollArea::vertical().id_salt("ds_list").auto_shrink([false, false]).show(ui, |ui| {
            for (i, item) in self.items.iter().enumerate() {
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::click());
                if i == self.selected {
                    ui.painter().rect_filled(rect, 4.0, p.selection);
                } else if resp.hovered() {
                    ui.painter().rect_filled(rect, 4.0, p.row_hover);
                }
                let ico = Rect::from_min_size(pos2(rect.left() + 8.0, rect.center().y - 8.0), vec2(16.0, 16.0));
                icons::paint(ui.painter(), ico, Ico::Driver(item.cfg.driver), p.icon, p, true);
                let name = if item.cfg.name.trim().is_empty() { tr("<unnamed>") } else { item.cfg.name.as_str() };
                let text_rect = Rect::from_min_max(pos2(ico.right() + 8.0, rect.top()), rect.max);
                ui.painter().with_clip_rect(text_rect).text(
                    pos2(text_rect.left(), rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    name,
                    egui::FontId::proportional(13.0),
                    p.text,
                );
                if resp.clicked() {
                    clicked = Some(i);
                }
            }
        });
        if let Some(i) = clicked {
            self.select(i);
        }
    }

    /// Right pane: Name, the tabs and the selected tab.
    fn details(&mut self, ui: &mut Ui, p: &Palette) {
        let driver = self.items[self.selected].cfg.driver;
        if !Self::has_tab(driver, self.tab) {
            self.tab = Tab::General;
        }
        egui::Grid::new("ds_name").num_columns(2).spacing([14.0, 10.0]).min_col_width(LABEL_WIDTH - 14.0).show(ui, |ui| {
            ui.label(tr("Name:"));
            text_field(ui, &mut self.items[self.selected].cfg.name, "");
            ui.end_row();
        });
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let tabs = [(Tab::General, tr("General")), (Tab::Options, tr("Options")), (Tab::Ssh, "SSH/SSL"), (Tab::Advanced, tr("Advanced"))];
            for (tab, label) in tabs.into_iter().filter(|(t, _)| Self::has_tab(driver, *t)) {
                if theme::tab(ui, self.tab == tab, None, label, false).0.clicked() {
                    self.tab = tab;
                }
            }
        });
        ui.add_space(14.0);
        egui::ScrollArea::vertical().id_salt("ds_form").auto_shrink([false, false]).show(ui, |ui| match self.tab {
            Tab::General => self.general(ui, p),
            Tab::Options => self.options(ui),
            Tab::Ssh => {
                let driver = self.items[self.selected].cfg.driver;
                if driver.supports_ssh() {
                    self.ssh(ui, p);
                }
                if tls::supported(driver) {
                    ui.add_space(22.0);
                    self.ssl(ui, p);
                }
            }
            Tab::Advanced => self.properties(ui, p),
        });
    }

    fn general(&mut self, ui: &mut Ui, p: &Palette) {
        let item = &mut self.items[self.selected];
        let before = item.cfg.driver;
        egui::Grid::new("ds_general").num_columns(2).spacing([14.0, 10.0]).min_col_width(LABEL_WIDTH - 14.0).show(ui, |ui| {
            ui.label(tr("Driver:"));
            driver_combo(ui, &mut item.cfg, p);
            ui.end_row();

            if item.cfg.driver == Driver::Sqlite {
                ui.label(tr("File:"));
                sqlite_file(ui, &mut item.cfg, &mut self.error);
                ui.end_row();
            } else {
                ui.label(tr("Connection type:"));
                segmented(ui, &mut item.cfg.url_only, [(false, tr("default")), (true, tr("URL only"))], p);
                ui.end_row();

                if item.cfg.url_only {
                    url_row(ui, &mut item.cfg, &mut self.url, p);
                } else {
                    ui.label(tr("Host:"));
                    ui.horizontal(|ui| {
                        let host_w = (ui.available_width() - 190.0).max(160.0);
                        let hint = match item.cfg.driver {
                            Driver::Snowflake => "<account>.snowflakecomputing.com",
                            Driver::Kusto => "<cluster>.<region>.kusto.windows.net",
                            _ => "localhost",
                        };
                        ui.add(egui::TextEdit::singleline(&mut item.cfg.host).hint_text(hint).margin(egui::vec2(8.0, 5.0)).desired_width(host_w));
                        ui.add_space(14.0);
                        ui.label(tr("Port:"));
                        ui.add_sized([ui.available_width(), 28.0], egui::DragValue::new(&mut item.cfg.port).range(1..=65535).speed(0));
                    });
                    ui.end_row();
                }
                auth_rows(ui, &mut item.cfg);
            }

            if !item.cfg.url_only {
                match item.cfg.driver {
                    Driver::Sqlite => {}
                    Driver::Snowflake => {
                        for (label, value) in [
                            (tr("Role:"), &mut item.cfg.role),
                            (tr("Database:"), &mut item.cfg.database),
                            (tr("Schema:"), &mut item.cfg.schema),
                            (tr("Warehouse:"), &mut item.cfg.warehouse),
                        ] {
                            ui.label(label);
                            text_field(ui, value, "");
                            ui.end_row();
                        }
                    }
                    d => {
                        ui.label(match d {
                            Driver::Oracle => tr("Service:"),
                            Driver::InfluxDb => tr("Bucket:"),
                            _ => tr("Database:"),
                        });
                        text_field(ui, &mut item.cfg.database, "");
                        ui.end_row();
                    }
                }
                // DataGrip leaves a gap above the URL.
                ui.label("");
                ui.allocate_space(vec2(0.0, 4.0));
                ui.end_row();
                url_row(ui, &mut item.cfg, &mut self.url, p);
            }
        });
        if item.cfg.driver != before && !Self::has_tab(item.cfg.driver, self.tab) {
            self.tab = Tab::General;
        }
    }

    fn options(&mut self, ui: &mut Ui) {
        let cfg = &mut self.items[self.selected].cfg;
        ui.checkbox(&mut cfg.read_only, tr("Read-only"))
            .on_hover_text(tr("Only queries can run; the data editor and the create, drop and rename actions are disabled"));
        ui.add_space(6.0);
        ui.checkbox(&mut cfg.confirm_dangerous, tr("Confirm dangerous statements"))
            .on_hover_text(tr("The console asks before UPDATE or DELETE without WHERE, TRUNCATE and DROP"));
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.label(tr("Environment:"));
            egui::ComboBox::from_id_salt("ds_environment").selected_text(theme::environment_name(cfg.environment)).show_ui(ui, |ui| {
                for env in Environment::ALL {
                    ui.selectable_value(&mut cfg.environment, env, theme::environment_name(env));
                }
            });
            if let Some((env, _)) = theme::environment(cfg) {
                let own = env.default_color();
                let mut rgb = cfg.environment_color.unwrap_or(own);
                if ui.color_edit_button_srgb(&mut rgb).on_hover_text(tr("Badge color")).changed() {
                    cfg.environment_color = (rgb != own).then_some(rgb);
                }
                if cfg.environment_color.is_some() && ui.small_button(tr("Reset")).clicked() {
                    cfg.environment_color = None;
                }
                if let Some((env, color)) = theme::environment(cfg) {
                    ui.add_space(6.0);
                    theme::environment_badge(ui, env, color);
                }
            }
        });
        if cfg.environment == Environment::Production {
            ui.add_space(4.0);
            ui.label(RichText::new(tr("Statements that change data and grid saves ask for confirmation first")).small().color(theme::pal(ui).muted));
        }
        if cfg.driver == Driver::MsSql {
            ui.add_space(6.0);
            ui.checkbox(&mut cfg.trust_cert, tr("Trust server certificate"));
        }
        // Redis / Valkey over TLS (`rediss://`); MongoDB takes `?tls=true` in its URL. ClickHouse
        // has its own secure port, followed when it was on the default one; InfluxDB serves
        // HTTPS on the same port. Kusto is HTTPS unless turned off (the emulator serves HTTP).
        if matches!(cfg.driver, Driver::Redis | Driver::ClickHouse | Driver::InfluxDb | Driver::Kusto) {
            ui.add_space(6.0);
            let kusto = cfg.driver == Driver::Kusto;
            let mut tls = cfg.property("tls").map_or(kusto, |v| v.eq_ignore_ascii_case("true"));
            if ui.checkbox(&mut tls, tr("Use TLS")).changed() {
                cfg.set_property("tls", match (tls, kusto) {
                    (true, false) => "true",
                    (false, true) => "false",
                    _ => "",
                });
                if cfg.driver == Driver::ClickHouse {
                    let (plain, secure) = (Driver::ClickHouse.default_port(), crate::db::clickhouse::TLS_PORT);
                    match (tls, cfg.port) {
                        (true, p) if p == plain => cfg.port = secure,
                        (false, p) if p == secure => cfg.port = plain,
                        _ => {}
                    }
                }
            }
            if tls && matches!(cfg.driver, Driver::ClickHouse | Driver::InfluxDb) {
                let mut insecure = cfg.property("tls_insecure").is_some_and(|v| v.eq_ignore_ascii_case("true"));
                if ui.checkbox(&mut insecure, tr("Trust server certificate")).changed() {
                    cfg.set_property("tls_insecure", if insecure { "true" } else { "" });
                }
            }
        }
    }

    /// SSL part of the SSH/SSL tab (PostgreSQL, MySQL, MariaDB): the mode and the files, read on
    /// every connect.
    fn ssl(&mut self, ui: &mut Ui, p: &Palette) {
        let t = &mut self.items[self.selected].cfg.tls;
        ui.label(RichText::new("SSL").strong());
        ui.add_space(8.0);
        egui::Grid::new("ds_ssl").num_columns(2).spacing([14.0, 10.0]).min_col_width(LABEL_WIDTH - 14.0).show(ui, |ui| {
            ui.label(tr("Mode:"));
            ui.vertical(|ui| {
                egui::ComboBox::from_id_salt("ds_ssl_mode").icon(chevron).selected_text(t.mode.label()).width(ui.available_width()).show_ui(ui, |ui| {
                    for m in TlsMode::ALL {
                        ui.selectable_value(&mut t.mode, m, m.label()).on_hover_text(m.hint());
                    }
                });
                ui.label(RichText::new(t.mode.hint()).size(12.0).color(p.muted));
            });
            ui.end_row();
            let on = t.mode != TlsMode::Disable;
            for (label, file, title, hint) in [
                (tr("CA file:"), &mut t.ca_file, tr("CA Certificate"), tr("<system certificates>")),
                (tr("Client certificate:"), &mut t.cert_file, tr("Client Certificate"), ""),
                (tr("Client key:"), &mut t.key_file, tr("Client Key"), ""),
            ] {
                ui.add_enabled(on, egui::Label::new(label));
                ui.add_enabled_ui(on, |ui| file_row(ui, file, title, hint));
                ui.end_row();
            }
            ui.add_enabled(on, egui::Label::new(tr("Key passphrase:")));
            ui.add_enabled_ui(on, |ui| password_field(ui, &mut t.key_passphrase));
            ui.end_row();
        });
        ui.add_space(8.0);
        ui.label(
            RichText::new(tr("The CA file is used by Verify CA and Verify full. The client certificate and key are only needed when the server signs users in by certificate."))
                .size(12.0)
                .color(p.muted),
        );
    }

    /// SSH tab: the tunnel through a bastion. The General tab's host and port stay the database's
    /// address as the bastion sees it. A key the bastion showed and isn't trusted yet is offered
    /// here with its fingerprint.
    fn ssh(&mut self, ui: &mut Ui, p: &Palette) {
        let s = &mut self.items[self.selected].cfg.ssh;
        ui.checkbox(&mut s.enabled, tr("Use SSH tunnel"));
        ui.add_space(10.0);
        ui.add_enabled_ui(s.enabled, |ui| {
            egui::Grid::new("ds_ssh").num_columns(2).spacing([14.0, 10.0]).min_col_width(LABEL_WIDTH - 14.0).show(ui, |ui| {
                ui.label(tr("Host:"));
                ui.horizontal(|ui| {
                    let host_w = (ui.available_width() - 190.0).max(160.0);
                    ui.add(egui::TextEdit::singleline(&mut s.host).hint_text("bastion.example.com").margin(egui::vec2(8.0, 5.0)).desired_width(host_w));
                    ui.add_space(14.0);
                    ui.label(tr("Port:"));
                    let mut port = s.port();
                    if ui.add_sized([ui.available_width(), 28.0], egui::DragValue::new(&mut port).range(1..=65535).speed(0)).changed() {
                        s.port = port;
                    }
                });
                ui.end_row();

                ui.label(tr("User:"));
                text_field(ui, &mut s.user, "");
                ui.end_row();

                let label = |a: SshAuth| match a {
                    SshAuth::Password => tr("Password"),
                    SshAuth::KeyPair => tr("Key pair"),
                    SshAuth::Agent => tr("SSH agent"),
                };
                ui.label(tr("Authentication:"));
                egui::ComboBox::from_id_salt("ds_ssh_auth").icon(chevron).selected_text(label(s.auth)).width(ui.available_width()).show_ui(ui, |ui| {
                    for a in [SshAuth::Password, SshAuth::KeyPair, SshAuth::Agent] {
                        ui.selectable_value(&mut s.auth, a, label(a));
                    }
                });
                ui.end_row();

                match s.auth {
                    SshAuth::Password => {
                        ui.label(tr("Password:"));
                        password_field(ui, &mut s.password);
                        ui.end_row();
                    }
                    SshAuth::KeyPair => {
                        ui.label(tr("Private key file:"));
                        ui.horizontal(|ui| {
                            let w = (ui.available_width() - 90.0).max(160.0);
                            ui.add(egui::TextEdit::singleline(&mut s.key_file).hint_text("~/.ssh/id_ed25519").margin(egui::vec2(8.0, 5.0)).desired_width(w));
                            if ui.button(tr("Browse…")).clicked() {
                                let mut picker = rfd::FileDialog::new().set_title(tr("SSH Private Key"));
                                if let Some(dir) = dirs::home_dir().map(|h| h.join(".ssh")).filter(|d| d.is_dir()) {
                                    picker = picker.set_directory(dir);
                                }
                                if let Some(file) = picker.pick_file() {
                                    s.key_file = file.to_string_lossy().into_owned();
                                }
                            }
                        });
                        ui.end_row();
                        ui.label(tr("Passphrase:"));
                        password_field(ui, &mut s.passphrase);
                        ui.end_row();
                    }
                    SshAuth::Agent => {}
                }
            });
            ui.add_space(12.0);
            ui.label(
                RichText::new(tr("Host and port in General are the database's address as the SSH server reaches it (often localhost). Passwords and passphrases are kept in the system keychain."))
                    .size(12.0)
                    .color(p.muted),
            );
            if let Some(fingerprint) = ssh::unknown_key(s) {
                ui.add_space(14.0);
                ui.label(RichText::new(trf("The SSH server {host} isn't trusted yet. Its key:", &[("host", &s.host.trim())])).strong());
                ui.add(egui::Label::new(RichText::new(&fingerprint).monospace()).selectable(true));
                ui.label(RichText::new(tr("Compare it with the server's (ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub on the server) before trusting it.")).size(12.0).color(p.muted));
                ui.add_space(4.0);
                if ui.button(tr("Trust")).clicked() {
                    match ssh::trust(s) {
                        Ok(()) => self.retest = true,
                        Err(e) => self.error = Some(format!("{e:#}")),
                    }
                }
            }
        });
    }

    /// Advanced tab: DataGrip's Name / Value table of user-defined driver properties. The last
    /// row is always empty; typing in it adds a property, and "−" removes one.
    fn properties(&mut self, ui: &mut Ui, p: &Palette) {
        const ROW_H: f32 = 30.0;
        let cfg = &mut self.items[self.selected].cfg;
        let line = Stroke::new(1.0, p.grid);
        let width = ui.available_width();
        let name_w = (width * 0.5).floor();
        let mut remove = None;

        let header = |ui: &mut Ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(width, ROW_H), Sense::hover());
            ui.painter().rect_filled(rect, 0.0, p.header_bg);
            for (x, text) in [(rect.left(), tr("Name")), (rect.left() + name_w, tr("Value"))] {
                ui.painter().text(pos2(x + 10.0, rect.center().y), egui::Align2::LEFT_CENTER, text, egui::FontId::proportional(13.0), p.text);
            }
            ui.painter().vline(rect.left() + name_w, rect.y_range(), line);
            ui.painter().hline(rect.x_range(), rect.bottom(), line);
        };
        // One row: two frameless editors split at the column line, and "−" on hover.
        let row = |ui: &mut Ui, i: usize, name: &mut String, value: &mut String, placeholder: bool| -> (bool, bool) {
            let (rect, resp) = ui.allocate_exact_size(vec2(width, ROW_H), Sense::hover());
            let name_rect = Rect::from_min_size(rect.min, vec2(name_w, ROW_H));
            let value_rect = Rect::from_min_max(pos2(rect.left() + name_w, rect.top()), pos2(rect.right() - 28.0, rect.bottom()));
            let edit = |ui: &mut Ui, r: Rect, id: (&str, usize), v: &mut String, hint: &str, secret: bool| {
                ui.put(
                    r.shrink2(vec2(2.0, 3.0)),
                    egui::TextEdit::singleline(v)
                        .id_salt(id)
                        .frame(egui::Frame::NONE)
                        .hint_text(hint)
                        .password(secret)
                        .margin(egui::vec2(8.0, 4.0))
                        .vertical_align(Align::Center),
                )
            };
            let (nh, vh) = if placeholder { (tr("<user defined>"), tr("<value>")) } else { ("", "") };
            // Secret values (kept in the keychain) are masked like the Password field.
            let secret = is_secret_property(name);
            let a = edit(ui, name_rect, ("prop_name", i), name, nh, false);
            let b = edit(ui, value_rect, ("prop_value", i), value, vh, secret);
            ui.painter().vline(rect.left() + name_w, rect.y_range(), line);
            ui.painter().hline(rect.x_range(), rect.bottom(), line);
            let mut removed = false;
            if !placeholder && (resp.hovered() || a.has_focus() || b.has_focus() || ui.rect_contains_pointer(rect)) {
                let r = Rect::from_center_size(pos2(rect.right() - 14.0, rect.center().y), vec2(22.0, 22.0));
                removed = ui.put(r, |ui: &mut Ui| theme::icon_button_sized(ui, Ico::Minus, true, tr("Remove"), 22.0)).clicked();
            }
            (a.changed() || b.changed(), removed)
        };

        ui.spacing_mut().item_spacing.y = 0.0;
        let top = ui.cursor().top();
        header(ui);
        for (i, (name, value)) in cfg.properties.iter_mut().enumerate() {
            if row(ui, i, name, value, false).1 {
                remove = Some(i);
            }
        }
        // The new row has the ids the property will have once added, so focus stays in it.
        let n = cfg.properties.len();
        let (mut name, mut value) = (String::new(), String::new());
        if row(ui, n, &mut name, &mut value, true).0 && !(name.is_empty() && value.is_empty()) {
            cfg.properties.push((name, value));
        }
        let table = Rect::from_min_max(pos2(ui.min_rect().left(), top), pos2(ui.min_rect().left() + width, ui.cursor().top()));
        ui.painter().rect_stroke(table, 4.0, line, egui::StrokeKind::Inside);
        if let Some(i) = remove {
            cfg.properties.remove(i);
        }
        ui.spacing_mut().item_spacing.y = 8.0;
        ui.add_space(16.0);
        ui.label(RichText::new(tr("User defined driver property. Key pair: set private_key_file (and private_key_file_pwd for an encrypted key) with Authentication → Authenticator. Secret values (passwords, passphrases, tokens) are kept in the system keychain, not in the connections file.")).size(12.0).color(p.muted));
    }

    fn open_import(&mut self, files: Vec<std::path::PathBuf>) {
        let mut import = Import { file: None, found: Vec::new(), error: None, files };
        if let Some(first) = import.files.first().cloned() {
            self.load_import(&mut import, first);
        }
        self.import = Some(import);
    }

    /// Reads a `dataSources.xml`: the data sources DataCraft can open are ticked, unless one like
    /// it is already in the list.
    fn load_import(&self, import: &mut Import, file: std::path::PathBuf) {
        match datagrip::read(&file) {
            Ok(found) => {
                import.found = found
                    .into_iter()
                    .map(|f| {
                        let tick = f.config.as_ref().is_ok_and(|c| !self.items.iter().any(|i| same_target(&i.cfg, c)));
                        (f, tick)
                    })
                    .collect();
                import.error = None;
            }
            Err(e) => {
                import.found.clear();
                import.error = Some(format!("{e:#}"));
            }
        }
        import.file = Some(file);
    }

    /// The import window; true when it closes.
    fn import_window(&mut self, ctx: &egui::Context, p: &Palette) -> bool {
        let Some(mut import) = self.import.take() else { return true };
        let mut close = false;
        let mut load = None;
        egui::Modal::new(egui::Id::new("import_datagrip")).frame(confirm_frame(ctx, p)).show(ctx, |ui| {
            ui.set_width(620.0);
            ui.label(RichText::new(tr("Import from DataGrip")).font(theme::bold(15.0)));
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let shown = |f: &std::path::Path| home_relative(f);
                let current = import.file.as_deref().map(shown).unwrap_or_default();
                egui::ComboBox::from_id_salt("import_file").icon(chevron).selected_text(current).width(ui.available_width() - 90.0).show_ui(ui, |ui| {
                    for f in &import.files {
                        if ui.selectable_label(import.file.as_ref() == Some(f), shown(f)).clicked() {
                            load = Some(f.clone());
                        }
                    }
                });
                if ui.button(tr("Browse…")).clicked()
                    && let Some(f) = rfd::FileDialog::new().set_title(tr("DataGrip data sources")).add_filter("dataSources.xml", &["xml"]).pick_file()
                {
                    if !import.files.contains(&f) {
                        import.files.push(f.clone());
                    }
                    load = Some(f);
                }
            });
            ui.add_space(8.0);
            if import.file.is_none() {
                ui.label(RichText::new(tr("No DataGrip projects found. Choose a project's .idea/dataSources.xml file.")).color(p.muted));
            } else if let Some(e) = &import.error {
                ui.label(RichText::new(e).color(p.error));
            } else if import.found.is_empty() {
                ui.label(RichText::new(tr("This file has no data sources.")).color(p.muted));
            }
            egui::ScrollArea::vertical().max_height(320.0).auto_shrink([false, true]).show(ui, |ui| {
                for (f, tick) in &mut import.found {
                    ui.horizontal(|ui| {
                        ui.add_enabled(f.config.is_ok(), egui::Checkbox::new(tick, &f.name));
                        if let Ok(c) = &f.config {
                            let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
                            icons::paint(ui.painter(), r, Ico::Driver(c.driver), p.icon, p, true);
                        }
                        let mut detail = match &f.config {
                            Ok(c) => format!("{} · {}", c.driver.label(), target(c)),
                            Err(e) => e.clone(),
                        };
                        if !f.folder.is_empty() {
                            detail = format!("{} · {detail}", f.folder);
                        }
                        if f.config.as_ref().is_ok_and(|c| self.items.iter().any(|i| same_target(&i.cfg, c))) {
                            detail = format!("{detail} · {}", tr("already added"));
                        }
                        ui.add(egui::Label::new(RichText::new(detail).size(12.0).color(p.muted)).truncate());
                    });
                }
            });
            ui.add_space(10.0);
            ui.label(RichText::new(tr("Passwords aren't imported: DataCraft asks for each one on the first connect. DataGrip's files are only read.")).size(12.0).color(p.muted));
            ui.add_space(14.0);
            let count = import.found.iter().filter(|(_, t)| *t).count();
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let label = trf("Import {count}", &[("count", &count)]);
                if ui.add_enabled_ui(count > 0, |ui| theme::primary_button(ui, &label)).inner.clicked() {
                    let first = self.items.len();
                    for (f, _) in import.found.iter().filter(|(_, t)| *t) {
                        let Ok(mut cfg) = f.config.clone() else { continue };
                        cfg.id = self.next_id();
                        // Never over an existing one: a name in use gets " (copy)".
                        while self.items.iter().any(|i| i.cfg.name == cfg.name) {
                            cfg.name = trf("{name} (copy)", &[("name", &cfg.name)]);
                        }
                        self.items.push(Item { cfg, saved: None });
                    }
                    self.select(first);
                    close = true;
                }
                if theme::secondary_button(ui, tr("Cancel")).clicked() {
                    close = true;
                }
            });
        });
        if let Some(f) = load {
            self.load_import(&mut import, f);
        }
        self.import = Some(import);
        close
    }

    /// "Test Connection" and its result, as in DataGrip's footer of the details pane.
    fn test_row(&mut self, ui: &mut Ui, p: &Palette) -> Option<DialogResult> {
        if self.items.is_empty() {
            return None;
        }
        let cfg = &self.items[self.selected].cfg;
        let mine = self.test.0 == cfg.id;
        let testing = mine && matches!(self.test.1, TestState::Testing);
        let mut result = None;
        let link = ui.add_enabled(!testing, egui::Button::new(RichText::new(tr("Test Connection")).color(p.accent)).frame(false));
        if link.clicked() || std::mem::take(&mut self.retest) {
            match Self::validate(cfg) {
                Ok(()) => {
                    self.error = None;
                    self.test = (cfg.id, TestState::Testing);
                    result = Some(DialogResult::Test(Box::new(Self::finished(cfg))));
                }
                Err(e) => self.error = Some(e),
            }
        }
        if !mine {
            return result;
        }
        match &self.test.1 {
            TestState::Idle => {}
            TestState::Testing => {
                ui.add(egui::Spinner::new().size(14.0));
            }
            TestState::Done(Ok(version)) => {
                mark(ui, true, p);
                ui.label(RichText::new(version.as_deref().unwrap_or(tr("Succeeded"))).color(p.muted));
            }
            TestState::Done(Err(e)) => {
                mark(ui, false, p);
                // The bastion's key isn't trusted yet: the SSH tab shows it.
                if ssh::tunnels(cfg) && ssh::unknown_key(&cfg.ssh).is_some() && self.tab != Tab::Ssh && ui.button(tr("Review SSH key")).clicked() {
                    self.tab = Tab::Ssh;
                }
                let first = e.lines().next().unwrap_or(e);
                ui.add(egui::Label::new(RichText::new(first).color(p.error)).truncate()).on_hover_text(e);
            }
        }
        result
    }
}

/// Same engine and address (or SQLite file), user and database: an import of a data source the
/// list already has.
fn same_target(a: &ConnectionConfig, b: &ConnectionConfig) -> bool {
    a.driver == b.driver
        && if a.driver == Driver::Sqlite {
            a.file_path.trim() == b.file_path.trim()
        } else {
            a.host.trim().eq_ignore_ascii_case(b.host.trim()) && a.port == b.port && a.database.trim() == b.database.trim() && a.user.trim() == b.user.trim()
        }
}

/// `user@host:port/database`, or the SQLite file.
fn target(c: &ConnectionConfig) -> String {
    if c.driver == Driver::Sqlite {
        return home_relative(std::path::Path::new(&c.file_path));
    }
    let user = if c.user.trim().is_empty() { String::new() } else { format!("{}@", c.user.trim()) };
    let db = if c.database.trim().is_empty() { String::new() } else { format!("/{}", c.database.trim()) };
    format!("{user}{}:{}{db}", c.host.trim(), c.port)
}

/// A path with the home folder as `~`.
fn home_relative(path: &std::path::Path) -> String {
    match dirs::home_dir().and_then(|h| path.strip_prefix(h).ok().map(|r| r.to_path_buf())) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

fn confirm_frame(ctx: &egui::Context, p: &Palette) -> egui::Frame {
    egui::Frame::popup(&ctx.global_style())
        .fill(p.popup_bg)
        .corner_radius(12)
        .inner_margin(egui::Margin::same(20))
        .stroke(Stroke::new(1.0, p.popup_border))
}

/// DataGrip's green check / red cross after Test Connection (painted: the UI font has neither).
fn mark(ui: &mut Ui, ok: bool, p: &Palette) {
    let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
    let at = |x: f32, y: f32| pos2(r.left() + x, r.top() + y);
    if ok {
        ui.painter().add(egui::Shape::line(vec![at(2.5, 8.5), at(6.5, 12.5), at(13.5, 4.0)], Stroke::new(2.0, p.ok)));
    } else {
        let st = Stroke::new(2.0, p.error);
        ui.painter().line_segment([at(3.5, 3.5), at(12.5, 12.5)], st);
        ui.painter().line_segment([at(12.5, 3.5), at(3.5, 12.5)], st);
    }
}

/// IntelliJ's chevron instead of egui's triangle on combo boxes.
fn chevron(ui: &Ui, rect: Rect, _: &egui::style::WidgetVisuals, _open: bool) {
    let p = theme::pal(ui);
    icons::paint(ui.painter(), Rect::from_center_size(rect.center(), vec2(14.0, 14.0)), Ico::ChevronDown, p.muted, &p, true);
}

/// A file path with Browse….
fn file_row(ui: &mut Ui, file: &mut String, title: &str, hint: &str) {
    ui.horizontal(|ui| {
        let w = (ui.available_width() - 90.0).max(160.0);
        ui.add(egui::TextEdit::singleline(file).hint_text(hint).margin(egui::vec2(8.0, 5.0)).desired_width(w));
        if ui.button(tr("Browse…")).clicked()
            && let Some(picked) = rfd::FileDialog::new().set_title(title).pick_file()
        {
            *file = picked.to_string_lossy().into_owned();
        }
    });
}

fn password_field(ui: &mut Ui, value: &mut String) -> egui::Response {
    let hint = if value.is_empty() { tr("<empty>") } else { "" };
    ui.add(egui::TextEdit::singleline(value).password(true).hint_text(hint).margin(egui::vec2(8.0, 5.0)).desired_width(f32::INFINITY))
}

fn text_field(ui: &mut Ui, value: &mut String, hint: &str) -> egui::Response {
    ui.add(egui::TextEdit::singleline(value).hint_text(hint).margin(egui::vec2(8.0, 5.0)).desired_width(f32::INFINITY))
}

/// Full-width driver combo with the driver's icon, as DataGrip's.
fn driver_combo(ui: &mut Ui, cfg: &mut ConnectionConfig, p: &Palette) {
    let mut driver = cfg.driver;
    let resp = egui::ComboBox::from_id_salt("driver_combo")
        .icon(chevron)
        .selected_text(format!("      {}", driver.label()))
        .width(ui.available_width())
        .show_ui(ui, |ui| {
            for d in Driver::ALL {
                ui.selectable_value(&mut driver, d, d.label());
            }
        });
    let r = resp.response.rect;
    icons::paint(ui.painter(), Rect::from_min_size(pos2(r.left() + 8.0, r.center().y - 8.0), vec2(16.0, 16.0)), Ico::Driver(cfg.driver), p.icon, p, true);
    cfg.change_driver(driver);
}

/// DataGrip's two-button segmented control ("default | URL only").
fn segmented<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: [(T, &str); 2], p: &Palette) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for (i, (v, label)) in options.into_iter().enumerate() {
            let galley = ui.painter().layout_no_wrap(label.to_owned(), egui::FontId::proportional(13.0), p.text);
            let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 32.0, 28.0), Sense::click());
            let radius = if i == 0 {
                egui::CornerRadius { nw: 6, sw: 6, ne: 0, se: 0 }
            } else {
                egui::CornerRadius { nw: 0, sw: 0, ne: 6, se: 6 }
            };
            let fill = if *value == v { p.tab_selected_bg } else if resp.hovered() { p.hover } else { p.control_bg };
            let stroke = if *value == v { p.tab_selected_border } else { p.control_border };
            ui.painter().rect(rect, radius, fill, Stroke::new(1.0, stroke), egui::StrokeKind::Inside);
            ui.painter().galley(rect.center() - galley.size() / 2.0, galley, p.text);
            if resp.clicked() {
                *value = v;
            }
        }
    });
}

/// Authentication and the fields it shows: User & Password, Authenticator (Snowflake) or No auth.
fn auth_rows(ui: &mut Ui, cfg: &mut ConnectionConfig) {
    let influx = cfg.driver == Driver::InfluxDb;
    let kusto = cfg.driver == Driver::Kusto;
    let label = |m: AuthMode| match m {
        AuthMode::UserPassword if influx => tr("Token"),
        AuthMode::UserPassword if kusto => tr("Application (client secret)"),
        AuthMode::UserPassword => tr("User & Password"),
        AuthMode::Authenticator => tr("Authenticator"),
        AuthMode::NoAuth => tr("No auth"),
        AuthMode::AzureCli => tr("Azure CLI"),
    };
    let modes: &[AuthMode] = if cfg.driver == Driver::Snowflake {
        &[AuthMode::UserPassword, AuthMode::Authenticator, AuthMode::NoAuth]
    } else if influx {
        &[AuthMode::UserPassword]
    } else if kusto {
        &[AuthMode::AzureCli, AuthMode::UserPassword, AuthMode::NoAuth]
    } else {
        &[AuthMode::UserPassword, AuthMode::NoAuth]
    };
    ui.label(tr("Authentication:"));
    egui::ComboBox::from_id_salt("ds_auth").icon(chevron).selected_text(label(cfg.auth)).width(ui.available_width()).show_ui(ui, |ui| {
        for &m in modes {
            ui.selectable_value(&mut cfg.auth, m, label(m));
        }
    });
    ui.end_row();

    if cfg.auth == AuthMode::Authenticator {
        ui.label(tr("Authenticator:"));
        text_field(ui, &mut cfg.authenticator, "");
        ui.end_row();
    }
    // Kusto: the Entra ID tenant (empty: the Azure CLI's own).
    if kusto && cfg.auth != AuthMode::NoAuth {
        ui.label(tr("Tenant:"));
        let mut tenant = cfg.property("tenant").unwrap_or_default().to_string();
        let hint = if cfg.auth == AuthMode::AzureCli { tr("<default>") } else { "<tenant>.onmicrosoft.com" };
        if text_field(ui, &mut tenant, hint).changed() {
            cfg.set_property("tenant", tenant.trim());
        }
        ui.end_row();
    }
    if !matches!(cfg.auth, AuthMode::NoAuth | AuthMode::AzureCli) {
        // InfluxDB: the organization (empty: the token's only one) and the API token. Kusto: an
        // Entra ID application's client ID and secret.
        ui.label(if influx { tr("Organization:") } else if kusto { tr("Client ID:") } else { tr("User:") });
        text_field(ui, &mut cfg.user, "");
        ui.end_row();

        ui.label(if influx { tr("Token:") } else if kusto { tr("Client secret:") } else { tr("Password:") });
        let hint = if cfg.password.is_empty() { tr("<empty>") } else { "" };
        ui.add(egui::TextEdit::singleline(&mut cfg.password).password(true).hint_text(hint).margin(egui::vec2(8.0, 5.0)).desired_width(f32::INFINITY));
        ui.end_row();
    }
}

/// The URL field, synced both ways with the fields above it.
fn url_row(ui: &mut Ui, cfg: &mut ConnectionConfig, url: &mut String, p: &Palette) {
    ui.label("URL:");
    ui.vertical(|ui| {
        let edit = text_field(ui, url, "");
        // A URL that doesn't parse (another driver's, a port over 65535…) leaves the fields as
        // they were; say so while it is being edited.
        let valid = !edit.has_focus() || apply_url(&mut cfg.clone(), url);
        if edit.changed() {
            apply_url(cfg, url);
        }
        if !edit.has_focus() {
            *url = jdbc_url(cfg);
        }
        if valid {
            ui.label(RichText::new(tr("Overrides settings above. A postgresql://, mysql://, mongodb://… URL can be pasted too, with its user and password.")).size(12.0).color(p.muted));
        } else {
            ui.label(RichText::new(trf("Not a valid {driver} URL: the fields keep their values", &[("driver", &cfg.driver.label())])).size(12.0).color(p.error));
        }
    });
    ui.end_row();
}

fn sqlite_file(ui: &mut Ui, cfg: &mut ConnectionConfig, error: &mut Option<String>) {
    ui.horizontal(|ui| {
        let w = (ui.available_width() - 150.0).max(160.0);
        ui.add(egui::TextEdit::singleline(&mut cfg.file_path).margin(egui::vec2(8.0, 5.0)).desired_width(w));
        if ui.button(tr("Browse…")).clicked()
            && let Some(p) = rfd::FileDialog::new()
                .set_title(tr("Open SQLite Database"))
                .add_filter("SQLite", &["db", "sqlite", "sqlite3", "db3"])
                .add_filter(tr("All files"), &["*"])
                .pick_file()
        {
            cfg.file_path = p.to_string_lossy().into_owned();
        }
        if ui.button(tr("New…")).on_hover_text(tr("Create a new file")).clicked()
            && let Some(p) = rfd::FileDialog::new()
                .set_title(tr("New SQLite Database"))
                .add_filter("SQLite", &["db", "sqlite", "sqlite3"])
                .set_file_name(tr("new.db"))
                .save_file()
        {
            // The driver no longer creates files: it is created empty here (SQLite
            // initializes it on open). An existing file is reused.
            match std::fs::OpenOptions::new().write(true).create(true).truncate(false).open(&p) {
                Ok(_) => {
                    cfg.file_path = p.to_string_lossy().into_owned();
                    *error = None;
                }
                Err(e) => *error = Some(trf("Could not create {path}: {error}", &[("path", &p.display()), ("error", &e)])),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    //! Renders the window to PNGs in `target/dialog-snapshots/` (needs a GPU, hence ignored):
    //! `cargo test render_dialog -- --ignored`.

    use super::*;
    use egui_kittest::kittest::Queryable;

    #[test]
    fn ssh_tab_and_its_checks() {
        for d in Driver::ALL {
            assert_eq!(ConnectionDialog::has_tab(d, Tab::Ssh), !matches!(d, Driver::Sqlite | Driver::Snowflake | Driver::Kusto), "{d:?}");
        }
        let mut cfg = ConnectionConfig::new(1, Driver::Postgres);
        cfg.name = "db".into();
        assert!(ConnectionDialog::validate(&cfg).is_ok());
        cfg.ssh.enabled = true;
        assert_eq!(ConnectionDialog::validate(&cfg).unwrap_err(), tr("The SSH host is required"));
        cfg.ssh.host = "bastion".into();
        assert_eq!(ConnectionDialog::validate(&cfg).unwrap_err(), tr("The SSH user is required"));
        cfg.ssh.user = "deploy".into();
        cfg.ssh.auth = SshAuth::KeyPair;
        assert_eq!(ConnectionDialog::validate(&cfg).unwrap_err(), tr("Choose the SSH private key file"));
        cfg.ssh.key_file = "~/.ssh/id_ed25519".into();
        assert!(ConnectionDialog::validate(&cfg).is_ok());

        // Switching to an engine without the tab leaves it.
        let mut dialog = ConnectionDialog::edit(1, &[cfg]);
        dialog.tab = Tab::Ssh;
        let mut h = harness(dialog);
        h.get_by_label("Use SSH tunnel");
        h.state_mut().items[0].cfg.change_driver(Driver::Sqlite);
        h.run_steps(2);
        assert!(h.state().tab == Tab::General);
    }

    #[test]
    fn switching_to_kusto_and_back() {
        let mut cfg = ConnectionConfig::new(1, Driver::Postgres);
        cfg.change_driver(Driver::Kusto);
        // A cluster address and the Azure CLI's account, not localhost and postgres.
        assert_eq!((cfg.host.as_str(), cfg.port, cfg.user.as_str(), cfg.auth), ("", 443, "", AuthMode::AzureCli));
        cfg.change_driver(Driver::MySql);
        assert_eq!((cfg.host.as_str(), cfg.port, cfg.user.as_str(), cfg.auth), ("localhost", 3306, "root", AuthMode::UserPassword));
        // What was typed stays.
        cfg.host = "db.internal".into();
        cfg.user = "app".into();
        cfg.change_driver(Driver::Kusto);
        assert_eq!((cfg.host.as_str(), cfg.user.as_str(), cfg.auth), ("db.internal", "app", AuthMode::UserPassword));
    }

    fn snowflake_source() -> ConnectionConfig {
        let mut cfg = ConnectionConfig::new(3, Driver::Snowflake);
        cfg.name = "@ACME-XY12345.snowflakecomputing.com [3]".into();
        cfg.host = "ACME-XY12345.snowflakecomputing.com".into();
        cfg.user = "JDOE".into();
        cfg.password = "secret".into();
        cfg.auth = AuthMode::Authenticator;
        cfg.set_property("private_key_file", "/Users/jdoe/.ssh/snowflake_key.p8");
        cfg
    }

    fn render(name: &str, dark: bool, dialog: ConnectionDialog, setup: impl FnOnce(&mut ConnectionDialog)) {
        let mut dialog = dialog;
        setup(&mut dialog);
        // Fonts installed in one pass are only there from the next one.
        let mut first = true;
        let mut harness = egui_kittest::Harness::builder().with_size(vec2(1100.0, 840.0)).with_pixels_per_point(2.0).build_ui_state(
            move |ui, d: &mut ConnectionDialog| {
                let ctx = ui.ctx().clone();
                if std::mem::take(&mut first) {
                    theme::install_fonts(&ctx);
                    theme::apply(&ctx, dark);
                } else {
                    d.show(&ctx);
                }
            },
            dialog,
        );
        harness.run_steps(6);
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/dialog-snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        harness.render().expect("render").save(dir.join(format!("{name}.png"))).unwrap();
    }

    #[test]
    #[ignore]
    fn render_dialog() {
        let others = |sf: ConnectionConfig| {
            let mut pg = ConnectionConfig::new(1, Driver::Postgres);
            pg.name = "orders@localhost".into();
            pg.host = "localhost".into();
            pg.database = "orders".into();
            let mut my = ConnectionConfig::new(2, Driver::MySql);
            my.name = "inventory@mysql.example.com".into();
            vec![pg, my, sf]
        };
        let list = others(snowflake_source());
        render("snowflake_general", true, ConnectionDialog::edit(3, &list), |d| {
            d.test = (3, TestState::Done(Ok(Some("Snowflake 10.37.102".into()))));
        });
        let mut user_password = snowflake_source();
        user_password.auth = AuthMode::UserPassword;
        user_password.properties.clear();
        render("snowflake_user_password", true, ConnectionDialog::edit(3, &others(user_password)), |_| {});
        render("snowflake_advanced", true, ConnectionDialog::edit(3, &list), |d| d.tab = Tab::Advanced);
        render("snowflake_light", false, ConnectionDialog::edit(3, &list), |_| {});
        render("postgres_general", true, ConnectionDialog::edit(1, &list), |_| {});
        render("new_url_only", true, ConnectionDialog::new_connection(9, &list), |d| d.items.last_mut().unwrap().cfg.url_only = true);
        let mut tunneled = list.clone();
        tunneled[0].ssh = ssh::SshSettings {
            enabled: true,
            host: "bastion.example.com".into(),
            user: "deploy".into(),
            auth: SshAuth::KeyPair,
            key_file: "~/.ssh/id_ed25519".into(),
            ..Default::default()
        };
        render("postgres_ssh", true, ConnectionDialog::edit(1, &tunneled), |d| {
            d.tab = Tab::Ssh;
            d.test = (1, TestState::Done(Ok(Some("PostgreSQL 17.2 · through SSH deploy@bastion.example.com".into()))));
        });
        tunneled[0].tls = tls::TlsSettings { mode: TlsMode::VerifyFull, ca_file: "~/certs/rds-ca.pem".into(), ..Default::default() };
        render("postgres_ssh_light", false, ConnectionDialog::edit(1, &tunneled), |d| d.tab = Tab::Ssh);
        let file = datagrip_fixture("render");
        render("import_datagrip", true, ConnectionDialog::edit(1, &list), |d| d.open_import(vec![file.clone()]));
    }

    /// A project's `.idea/dataSources.xml` in `target/`, with users in `dataSources.local.xml`.
    fn datagrip_fixture(name: &str) -> std::path::PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/datagrip-fixture").join(name).join(".idea");
        std::fs::create_dir_all(&dir).unwrap();
        let source = |name: &str, group: &str, uuid: &str, driver: &str, url: &str| {
            format!(r#"<data-source source="LOCAL" name="{name}" group="{group}" uuid="{uuid}"><driver-ref>{driver}</driver-ref><jdbc-url>{url}</jdbc-url></data-source>"#)
        };
        let sources = [
            source("orders@localhost", "", "a", "postgresql", "jdbc:postgresql://localhost:5432/orders"),
            source("billing", "Production", "b", "postgresql", "jdbc:postgresql://billing.example.com:5432/billing?sslmode=require"),
            source("shop", "Production", "c", "mysql.8", "jdbc:mysql://shop.example.com:3306/shop"),
            source("cache", "", "d", "redis", "jdbc:redis://cache.example.com:6379/0"),
            source("warehouse", "Analytics", "e", "redshift", "jdbc:redshift://rs.example.com:5439/dev"),
        ];
        std::fs::write(dir.join("dataSources.xml"), format!("<project version=\"4\"><component name=\"DataSourceManagerImpl\">{}</component></project>", sources.join(""))).unwrap();
        let local = r#"<project version="4"><component name="dataSourceStorageLocal"><data-source uuid="a"><user-name>postgres</user-name></data-source><data-source uuid="b"><user-name>billing_app</user-name></data-source></component></project>"#;
        std::fs::write(dir.join("dataSources.local.xml"), local).unwrap();
        dir.join("dataSources.xml")
    }

    /// The import adds the ticked data sources as new ones: an existing one is left unticked, a
    /// name in use gets " (copy)", and nothing is saved until OK.
    #[test]
    fn imports_from_datagrip() {
        let mut pg = ConnectionConfig::new(1, Driver::Postgres);
        pg.name = "shop".into();
        pg.database = "orders".into();
        let mut dialog = ConnectionDialog::edit(1, &[pg]);
        dialog.open_import(vec![datagrip_fixture("test")]);
        let ticks: Vec<bool> = dialog.import.as_ref().unwrap().found.iter().map(|(_, t)| *t).collect();
        assert_eq!(ticks, [false, true, true, true, false]);
        let mut h = harness(dialog);
        h.get_by_label(&trf("Import {count}", &[("count", &3)])).click();
        h.run_steps(3);
        let d = h.state();
        assert!(d.import.is_none());
        let names: Vec<&str> = d.items.iter().map(|i| i.cfg.name.as_str()).collect();
        assert_eq!(names, ["shop", "billing", "shop (copy)", "cache"]);
        assert!(d.items[1..].iter().all(|i| i.saved.is_none() && i.cfg.ask_password));
        assert_eq!((d.items[1].cfg.user.as_str(), d.items[1].cfg.tls.mode), ("billing_app", TlsMode::Require));
        assert_eq!(d.selected, 1);
    }

    fn harness(dialog: ConnectionDialog) -> egui_kittest::Harness<'static, ConnectionDialog> {
        let mut first = true;
        let mut h = egui_kittest::Harness::builder().with_size(vec2(1100.0, 840.0)).build_ui_state(
            move |ui, d: &mut ConnectionDialog| {
                let ctx = ui.ctx().clone();
                if std::mem::take(&mut first) {
                    theme::install_fonts(&ctx);
                    theme::apply(&ctx, true);
                } else {
                    d.show(&ctx);
                }
            },
            dialog,
        );
        h.run_steps(3);
        h
    }

    /// Drives the real widgets: the Authentication dropdown shows and hides its fields, the URL
    /// writes back into the fields, and the Advanced tab's empty row adds a property.
    #[test]
    fn fields_follow_authentication_and_url() {
        let mut cfg = snowflake_source();
        cfg.auth = AuthMode::UserPassword;
        cfg.properties.clear();
        let mut h = harness(ConnectionDialog::edit(3, &[cfg]));
        let cfg = |h: &egui_kittest::Harness<'_, ConnectionDialog>| h.state().items[0].cfg.clone();

        assert!(h.query_by_label("Authenticator:").is_none());
        assert!(h.query_by_label("User:").is_some());

        // The second combo box is Authentication (the first is Driver).
        let auth_combo = |h: &egui_kittest::Harness<'_, ConnectionDialog>| h.get_all_by_role(egui::accesskit::Role::ComboBox).nth(1).unwrap().click();
        auth_combo(&h);
        h.run_steps(3);
        h.get_by_label("Authenticator").click();
        h.run_steps(3);
        assert_eq!(cfg(&h).auth, AuthMode::Authenticator);
        assert!(h.query_by_label("Authenticator:").is_some());
        assert!(h.query_by_label("Password:").is_some());

        auth_combo(&h);
        h.run_steps(3);
        h.get_by_label("No auth").click();
        h.run_steps(3);
        assert_eq!(cfg(&h).auth, AuthMode::NoAuth);
        for gone in ["Authenticator:", "User:", "Password:"] {
            assert!(h.query_by_label(gone).is_none(), "{gone} still shown");
        }

        // URL → fields.
        let url = "jdbc:snowflake://ACME-XY12345.snowflakecomputing.com:443";
        h.get_all_by_value(url).next().unwrap().focus();
        h.run_steps(2);
        h.get_all_by_value(url).next().unwrap().type_text("?warehouse=COMPUTE_WH&role=ANALYST");
        h.run_steps(3);
        assert_eq!((cfg(&h).warehouse.as_str(), cfg(&h).role.as_str()), ("COMPUTE_WH", "ANALYST"));
        assert!(h.query_all_by_value("COMPUTE_WH").next().is_some(), "Warehouse field not updated");

        // Fields → URL: typing in Warehouse rebuilds the URL once it loses focus.
        h.get_all_by_value("COMPUTE_WH").next().unwrap().focus();
        h.run_steps(2);
        h.get_all_by_value("COMPUTE_WH").next().unwrap().type_text("2");
        h.run_steps(3);
        assert!(h.query_all_by_value(&format!("{url}?warehouse=COMPUTE_WH2&role=ANALYST")).next().is_some(), "URL not rebuilt");

        // Advanced: typing in the empty row adds a property.
        h.get_by_label("Advanced").click();
        h.run_steps(3);
        let n = h.get_all_by_role(egui::accesskit::Role::TextInput).count();
        assert!(n >= 2, "no property editors");
        h.get_all_by_role(egui::accesskit::Role::TextInput).nth(n - 2).unwrap().focus();
        h.run_steps(2);
        h.get_all_by_role(egui::accesskit::Role::TextInput).nth(n - 2).unwrap().type_text("private_key_file");
        h.run_steps(3);
        assert_eq!(cfg(&h).properties, vec![("private_key_file".to_string(), String::new())]);
        // Focus stayed in the new row: more typing extends the same property.
        h.get_all_by_value("private_key_file").next().unwrap().type_text("_pwd");
        h.run_steps(3);
        assert_eq!(cfg(&h).properties[0].0, "private_key_file_pwd");
    }

    /// Cancel / Escape with unapplied edits asks before discarding them (found in review of the
    /// Snowflake PR); with nothing changed it closes at once.
    #[test]
    fn cancel_asks_before_discarding_edits() {
        struct State {
            dialog: ConnectionDialog,
            closed: bool,
        }
        let mut first = true;
        let mut h = egui_kittest::Harness::builder().with_size(vec2(1100.0, 840.0)).build_ui_state(
            move |ui, st: &mut State| {
                let ctx = ui.ctx().clone();
                if std::mem::take(&mut first) {
                    theme::install_fonts(&ctx);
                    theme::apply(&ctx, true);
                } else if let Some(DialogResult::Cancel) = st.dialog.show(&ctx) {
                    st.closed = true;
                }
            },
            State { dialog: ConnectionDialog::edit(3, &[snowflake_source()]), closed: false },
        );
        h.run_steps(3);

        // Untouched: Escape closes.
        h.key_press(egui::Key::Escape);
        h.run_steps(2);
        assert!(h.state().closed);
        h.state_mut().closed = false;

        // Edited: Escape asks; Escape again goes back to editing.
        h.state_mut().dialog.items[0].cfg.warehouse = "WH".into();
        h.run_steps(2);
        h.key_press(egui::Key::Escape);
        h.run_steps(2);
        assert!(!h.state().closed);
        assert!(h.query_by_label("Unsaved changes").is_some());
        h.key_press(egui::Key::Escape);
        h.run_steps(2);
        assert!(!h.state().closed && h.query_by_label("Unsaved changes").is_none());

        // Cancel → Discard closes without saving.
        h.get_by_label("Cancel").click();
        h.run_steps(2);
        h.get_by_label("Discard").click();
        h.run_steps(2);
        assert!(h.state().closed);
    }
}
