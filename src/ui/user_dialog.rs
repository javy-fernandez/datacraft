//! "Users" / "Roles" editor: a modal dialog to create, edit or drop a MySQL / MariaDB user or a
//! PostgreSQL role, with the statements it will run in a preview (password masked). The SQL is
//! built in `db::users` as a diff between the loaded state and the edited one.

use crate::db::Driver;
use crate::db::users::{self, Account, PrivSet, PrivTarget, UserList, UserModel, UserState};
use crate::i18n::{tr, trf};
use crate::sql;
use crate::ui::theme::{self, palette};
use crate::worker::ConnId;
use egui::{FontId, Margin, RichText, Ui, vec2};

/// The "Users" / "Roles" folder of a connection in the explorer.
#[derive(Default)]
pub struct UsersNode {
    pub list: Option<UserList>,
    pub loading: bool,
    pub error: Option<String>,
}

/// What the dialog shows besides the user: the connection's databases, PostgreSQL schemas and
/// tables of its default database, and the list (other roles, the privileges the server has).
#[derive(Default)]
pub struct UserContext<'a> {
    pub databases: Vec<String>,
    /// PostgreSQL: schemas and tables of the connection's database.
    pub objects: Vec<PrivTarget>,
    pub list: Option<&'a UserList>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    General,
    Server,
    Objects,
    Membership,
}

pub enum UserDialogResult {
    /// Statements to run, with the real password.
    Apply(Vec<String>),
    Cancel,
}

pub struct UserDialog {
    pub conn: ConnId,
    pub driver: Driver,
    /// Read-only connection: everything can be seen, nothing can run.
    pub read_only: bool,
    /// Confirmation to drop `edit.account` instead of the editor.
    pub dropping: bool,
    /// The user as loaded (None = a new user).
    pub original: Option<UserState>,
    pub edit: UserState,
    /// Typed password and its confirmation: only sent to the worker inside the statements.
    password: String,
    confirm: String,
    /// Waiting for the user's detail.
    pub loading: bool,
    /// Account whose detail was requested (to match the reply).
    pub pending: Option<Account>,
    /// Statements sent to the worker; waiting for the reply.
    pub running: bool,
    pub error: Option<String>,
    tab: Tab,
    /// Object whose privileges are being edited.
    target: Option<PrivTarget>,
    focus_name: bool,
}

impl UserDialog {
    pub fn new_user(conn: ConnId, driver: Driver) -> Self {
        Self::make(conn, driver, None, UserState::new_user(driver), false)
    }

    /// Editor of an existing user, loading until `loaded` arrives.
    pub fn edit(conn: ConnId, driver: Driver, account: Account, read_only: bool) -> Self {
        let mut d = Self::make(
            conn,
            driver,
            None,
            UserState {
                account: account.clone(),
                ..UserState::new_user(driver)
            },
            read_only,
        );
        d.loading = true;
        d.pending = Some(account);
        d.focus_name = false;
        d
    }

    pub fn drop(conn: ConnId, driver: Driver, account: Account) -> Self {
        let mut d = Self::make(
            conn,
            driver,
            None,
            UserState {
                account,
                ..Default::default()
            },
            false,
        );
        d.dropping = true;
        d
    }

    fn make(
        conn: ConnId,
        driver: Driver,
        original: Option<UserState>,
        edit: UserState,
        read_only: bool,
    ) -> Self {
        Self {
            conn,
            driver,
            read_only,
            dropping: false,
            original,
            edit,
            password: String::new(),
            confirm: String::new(),
            loading: false,
            pending: None,
            running: false,
            error: None,
            tab: Tab::General,
            target: None,
            focus_name: true,
        }
    }

    fn pg(&self) -> bool {
        UserModel::of(self.driver) == Some(UserModel::Postgres)
    }

    /// The account the dialog is about (as loaded, or as typed for a new one).
    pub fn account(&self) -> &Account {
        self.original
            .as_ref()
            .map_or(&self.edit.account, |o| &o.account)
    }

    /// The user's detail arrived. `keep_edits`: after a failed apply, only the loaded state is
    /// replaced (what already ran is not run again) and the edits stay.
    pub fn loaded(&mut self, user: UserState, keep_edits: bool) {
        self.loading = false;
        if !keep_edits {
            self.edit = user.clone();
            self.target = self.edit.grants.keys().next().cloned();
        }
        self.original = Some(user);
    }

    pub fn load_failed(&mut self, error: String) {
        self.loading = false;
        self.error = Some(error);
    }

    /// Shows an error from the server without the password it may quote.
    pub fn set_error(&mut self, error: &str) {
        self.running = false;
        self.error = Some(users::mask_password(self.driver, error, &self.password));
    }

    /// The account as edited (after an apply that failed half-way it may already exist).
    pub fn edited_account(&self) -> Account {
        Account::new(self.edit.account.name.trim(), self.edit.account.host.trim())
    }

    fn password(&self) -> Option<&str> {
        Some(self.password.as_str()).filter(|p| !p.is_empty())
    }

    /// The statements; `reveal` puts the real password in them (only for the worker).
    pub fn statements(&self, reveal: bool) -> Vec<String> {
        if self.dropping {
            return vec![users::drop_statement(self.driver, &self.edit.account)];
        }
        let mut edit = self.edit.clone();
        edit.account.name = edit.account.name.trim().to_string();
        edit.account.host = edit.account.host.trim().to_string();
        edit.valid_until = edit.valid_until.trim().to_string();
        users::statements(
            self.driver,
            self.original.as_ref(),
            &edit,
            self.password(),
            reveal,
        )
    }

    fn validate(&self) -> Result<Vec<String>, String> {
        if self.dropping {
            return Ok(self.statements(true));
        }
        if self.edit.account.name.trim().is_empty() {
            return Err(tr("Name is required").into());
        }
        if !self.pg() && self.edit.account.host.trim().is_empty() {
            return Err(tr("Host is required").into());
        }
        if self.password != self.confirm {
            return Err(tr("The passwords do not match").into());
        }
        if let Some(o) = &self.original
            && !self.pg()
            && o.plugin.trim() != self.edit.plugin.trim()
            && !users::is_socket_plugin(&self.edit.plugin)
            && self.password.is_empty()
        {
            return Err(tr("Enter a password to change the authentication plugin").into());
        }
        Ok(self.statements(true))
    }

    pub fn show(&mut self, ctx: &egui::Context, uc: &UserContext) -> Option<UserDialogResult> {
        let mut result = None;
        let p = palette(ctx.global_style().visuals.dark_mode);
        let frame = egui::Frame::popup(&ctx.global_style())
            .fill(p.popup_bg)
            .corner_radius(12)
            .inner_margin(Margin::same(20))
            .stroke(egui::Stroke::new(1.0, p.popup_border));
        let pg = self.pg();
        let title = match (self.dropping, self.original.is_some() || self.loading, pg) {
            (true, _, false) => tr("Drop User"),
            (true, _, true) => tr("Drop Role"),
            (false, true, false) => tr("Edit User"),
            (false, true, true) => tr("Edit Role"),
            (false, false, false) => tr("New User"),
            (false, false, true) => tr("New Role"),
        };
        let label = self.account().label(self.driver);
        let preview = self.statements(false);
        let can_apply = !self.read_only && !self.loading && !self.running && !preview.is_empty();
        egui::Modal::new(egui::Id::new("user_dialog"))
            .frame(frame)
            .show(ctx, |ui| {
                ui.set_width(if self.dropping { 440.0 } else { 660.0 });
                ui.horizontal(|ui| {
                    ui.label(RichText::new(title).font(theme::bold(15.0)).color(p.text));
                    if self.original.is_some() || self.loading {
                        ui.add_space(6.0);
                        ui.label(RichText::new(&label).color(p.muted));
                    }
                    if self.read_only {
                        ui.add_space(6.0);
                        ui.label(RichText::new(tr("Read-only connection")).color(p.warn));
                    }
                });
                ui.add_space(12.0);
                if self.dropping {
                    let question = if pg {
                        trf("Drop role “{name}”?", &[("name", &label)])
                    } else {
                        trf("Drop user “{name}”?", &[("name", &label)])
                    };
                    ui.label(question);
                    ui.label(RichText::new(tr("This action cannot be undone.")).color(p.error));
                } else if self.loading {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(14.0).color(p.muted));
                        ui.label(RichText::new(tr("loading…")).color(p.muted));
                    });
                } else {
                    self.tabs(ui);
                    ui.add_space(10.0);
                    ui.add_enabled_ui(!self.read_only && !self.running, |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt("user_tab")
                            .min_scrolled_height(300.0)
                            .max_height(300.0)
                            .auto_shrink([false, false])
                            .show(ui, |ui| match self.tab {
                                Tab::General => self.general(ui),
                                Tab::Server => self.server_privileges(ui, uc),
                                Tab::Objects => self.object_privileges(ui, uc),
                                Tab::Membership => self.membership(ui, uc),
                            });
                    });
                }
                ui.add_space(12.0);
                sql_preview(ui, &preview);
                if let Some(e) = &self.error {
                    ui.add_space(8.0);
                    ui.add(egui::Label::new(RichText::new(e).monospace().color(p.error)).wrap());
                }
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if !self.dropping && theme::secondary_button(ui, tr("Copy SQL")).clicked() {
                        ui.ctx()
                            .copy_text(preview.iter().map(|s| format!("{s};\n")).collect());
                    }
                    if self.running {
                        ui.add_space(8.0);
                        ui.add(egui::Spinner::new().size(14.0).color(p.muted));
                        ui.label(
                            RichText::new(if self.dropping {
                                tr("Dropping…")
                            } else {
                                tr("Applying…")
                            })
                            .color(p.muted),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        if !self.read_only {
                            ui.add_enabled_ui(can_apply, |ui| {
                                if theme::primary_button(
                                    ui,
                                    if self.dropping {
                                        tr("Drop")
                                    } else {
                                        tr("Apply")
                                    },
                                )
                                .clicked()
                                {
                                    match self.validate() {
                                        Ok(statements) => {
                                            result = Some(UserDialogResult::Apply(statements))
                                        }
                                        Err(e) => self.error = Some(e),
                                    }
                                }
                            });
                        }
                        ui.add_enabled_ui(!self.running, |ui| {
                            let close = if self.read_only {
                                tr("Close")
                            } else {
                                tr("Cancel")
                            };
                            if theme::secondary_button(ui, close).clicked() {
                                result = Some(UserDialogResult::Cancel);
                            }
                        });
                    });
                });
            });
        if result.is_none() && !self.running && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            result = Some(UserDialogResult::Cancel);
        }
        result
    }

    fn tabs(&mut self, ui: &mut Ui) {
        let tabs: Vec<(Tab, &str)> = if self.pg() {
            vec![
                (Tab::General, tr("General")),
                (Tab::Objects, tr("Privileges")),
                (Tab::Membership, tr("Member Of")),
            ]
        } else {
            vec![
                (Tab::General, tr("General")),
                (Tab::Server, tr("Server Privileges")),
                (Tab::Objects, tr("Database Privileges")),
            ]
        };
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for (tab, text) in tabs {
                if theme::tab(ui, self.tab == tab, None, text, false)
                    .0
                    .clicked()
                {
                    self.tab = tab;
                }
            }
        });
    }

    fn general(&mut self, ui: &mut Ui) {
        let p = theme::pal(ui);
        let pg = self.pg();
        let driver = self.driver;
        let existing = self.original.is_some();
        let keep_hint = if existing {
            tr("Leave empty to keep the current one")
        } else {
            ""
        };
        egui::Grid::new("user_general")
            .num_columns(2)
            .spacing([14.0, 10.0])
            .min_col_width(130.0)
            .show(ui, |ui| {
                ui.label(tr("Name:"));
                let name = ui.add(
                    egui::TextEdit::singleline(&mut self.edit.account.name)
                        .margin(vec2(8.0, 5.0))
                        .desired_width(300.0),
                );
                if std::mem::take(&mut self.focus_name) {
                    name.request_focus();
                }
                ui.end_row();
                if !pg {
                    ui.label(tr("Host:"));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.edit.account.host)
                            .margin(vec2(8.0, 5.0))
                            .desired_width(300.0)
                            .hint_text("%"),
                    );
                    ui.end_row();
                    ui.label(tr("Authentication:"));
                    let plugin = &mut self.edit.plugin;
                    let shown = if plugin.is_empty() {
                        tr("Server default").to_string()
                    } else {
                        plugin.clone()
                    };
                    egui::ComboBox::from_id_salt("user_plugin")
                        .selected_text(shown)
                        .width(300.0)
                        .show_ui(ui, |ui| {
                            let mut options: Vec<&str> = users::auth_plugins(driver).to_vec();
                            let current = plugin.clone();
                            if !options.contains(&current.as_str()) {
                                options.push(&current);
                            }
                            for o in options {
                                let text = if o.is_empty() {
                                    tr("Server default")
                                } else {
                                    o
                                };
                                if ui.selectable_label(plugin.as_str() == o, text).clicked() {
                                    *plugin = o.to_string();
                                }
                            }
                        });
                    ui.end_row();
                }
                let no_password = !pg && users::is_socket_plugin(&self.edit.plugin);
                ui.label(tr("Password:"));
                ui.add_enabled(
                    !no_password,
                    egui::TextEdit::singleline(&mut self.password)
                        .password(true)
                        .margin(vec2(8.0, 5.0))
                        .desired_width(300.0)
                        .hint_text(keep_hint),
                );
                ui.end_row();
                ui.label(tr("Confirm password:"));
                ui.add_enabled(
                    !no_password,
                    egui::TextEdit::singleline(&mut self.confirm)
                        .password(true)
                        .margin(vec2(8.0, 5.0))
                        .desired_width(300.0),
                );
                ui.end_row();
                if pg {
                    ui.label(tr("Connection limit:"));
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::DragValue::new(&mut self.edit.conn_limit).range(-1..=1_000_000),
                        );
                        ui.label(RichText::new(tr("-1 = unlimited")).color(p.muted));
                    });
                    ui.end_row();
                    ui.label(tr("Valid until:"));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.edit.valid_until)
                            .margin(vec2(8.0, 5.0))
                            .desired_width(300.0)
                            .hint_text("2030-12-31 23:59:59+00"),
                    );
                    ui.end_row();
                } else {
                    ui.label(tr("Account:"));
                    ui.checkbox(&mut self.edit.locked, tr("Locked"));
                    ui.end_row();
                    let names = [
                        tr("Queries per hour:"),
                        tr("Updates per hour:"),
                        tr("Connections per hour:"),
                        tr("Simultaneous connections:"),
                    ];
                    for (i, name) in names.into_iter().enumerate() {
                        ui.label(name);
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::DragValue::new(&mut self.edit.limits[i])
                                    .range(0..=u32::MAX as u64),
                            );
                            if i == 0 {
                                ui.label(RichText::new(tr("0 = unlimited")).color(p.muted));
                            }
                        });
                        ui.end_row();
                    }
                }
            });
        if pg {
            ui.add_space(12.0);
            ui.label(RichText::new(tr("Attributes")).font(theme::bold(13.0)));
            ui.add_space(6.0);
            egui::Grid::new("role_attrs")
                .num_columns(4)
                .spacing([18.0, 8.0])
                .show(ui, |ui| {
                    for (i, attr) in users::ROLE_ATTRS.iter().enumerate() {
                        ui.checkbox(&mut self.edit.attrs[i], *attr);
                        if i % 4 == 3 {
                            ui.end_row();
                        }
                    }
                });
        }
    }

    fn server_privileges(&mut self, ui: &mut Ui, uc: &UserContext) {
        let names: Vec<String> = uc
            .list
            .map(|l| l.global_privileges.clone())
            .unwrap_or_default();
        privilege_grid(ui, "user_global", &names, &mut self.edit.global);
    }

    fn object_privileges(&mut self, ui: &mut Ui, uc: &UserContext) {
        let p = theme::pal(ui);
        let pg = self.pg();
        let mut candidates: Vec<PrivTarget> = uc
            .databases
            .iter()
            .map(|d| PrivTarget::Database(d.clone()))
            .collect();
        if pg {
            candidates.extend(uc.objects.iter().cloned());
        }
        candidates.retain(|t| !self.edit.grants.contains_key(t));
        let kind = |t: &PrivTarget| match t {
            PrivTarget::Database(_) => tr("database"),
            PrivTarget::Schema(_) => tr("schema"),
            PrivTarget::Table(..) => tr("table"),
        };
        ui.horizontal(|ui| {
            ui.label(tr("Add:"));
            let pick = egui::ComboBox::from_id_salt("user_add_target")
                .selected_text(tr("Choose an object…"))
                .width(320.0);
            pick.show_ui(ui, |ui| {
                for t in &candidates {
                    if ui
                        .selectable_label(false, format!("{} {}", kind(t), t.label()))
                        .clicked()
                    {
                        self.edit.grants.insert(t.clone(), PrivSet::default());
                        self.target = Some(t.clone());
                    }
                }
            });
        });
        if pg {
            ui.label(
                RichText::new(tr("Schemas and tables of the connection's database."))
                    .small()
                    .color(p.muted),
            );
        }
        ui.add_space(8.0);
        if self
            .target
            .as_ref()
            .is_none_or(|t| !self.edit.grants.contains_key(t))
        {
            self.target = self.edit.grants.keys().next().cloned();
        }
        let mut remove = None;
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(200.0);
                if self.edit.grants.is_empty() {
                    ui.label(RichText::new(tr("No privileges on any object")).color(p.muted));
                }
                for (t, set) in &self.edit.grants {
                    let text = format!("{} {}  ({})", kind(t), t.label(), set.privs.len());
                    if ui
                        .selectable_label(self.target.as_ref() == Some(t), text)
                        .clicked()
                    {
                        self.target = Some(t.clone());
                    }
                }
            });
            ui.separator();
            ui.vertical(|ui| {
                let Some(t) = self.target.clone() else { return };
                let names: Vec<String> = match &t {
                    PrivTarget::Database(_) if !pg => {
                        uc.list.map(|l| l.db_privileges.clone()).unwrap_or_default()
                    }
                    _ => t.pg_privileges().iter().map(|s| s.to_string()).collect(),
                };
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("{} {}", kind(&t), t.label()))
                            .font(theme::bold(13.0)),
                    );
                    if ui.small_button(tr("Remove")).clicked() {
                        remove = Some(t.clone());
                    }
                });
                ui.add_space(4.0);
                if let Some(set) = self.edit.grants.get_mut(&t) {
                    privilege_grid(ui, "user_target", &names, set);
                }
            });
        });
        if let Some(t) = remove {
            self.edit.grants.remove(&t);
        }
    }

    fn membership(&mut self, ui: &mut Ui, uc: &UserContext) {
        let p = theme::pal(ui);
        let me = self.original.as_ref().map(|o| o.account.name.clone());
        let mut roles: Vec<String> = uc
            .list
            .map(|l| l.accounts.iter().map(|a| a.account.name.clone()).collect())
            .unwrap_or_default();
        roles.extend(self.edit.member_of.iter().cloned());
        roles.sort();
        roles.dedup();
        roles.retain(|r| Some(r) != me.as_ref());
        if roles.is_empty() {
            ui.label(RichText::new(tr("There are no other roles")).color(p.muted));
        }
        for r in roles {
            let mut on = self.edit.member_of.contains(&r);
            if ui.checkbox(&mut on, &r).changed() {
                if on {
                    self.edit.member_of.insert(r);
                } else {
                    self.edit.member_of.remove(&r);
                }
            }
        }
    }
}

/// Checkbox grid of `names` (plus the grant option) on `set`. Privileges the set has that are not
/// in `names` (other server versions) are kept untouched.
fn privilege_grid(ui: &mut Ui, id: &str, names: &[String], set: &mut PrivSet) {
    ui.horizontal(|ui| {
        if ui.small_button(tr("Select All")).clicked() {
            set.privs.extend(names.iter().cloned());
        }
        if ui.small_button(tr("Clear")).clicked() {
            set.privs.retain(|p| !names.contains(p));
            set.grant_option = false;
        }
    });
    ui.add_space(4.0);
    egui::Grid::new(id)
        .num_columns(3)
        .spacing([18.0, 6.0])
        .show(ui, |ui| {
            for (i, name) in names.iter().enumerate() {
                let mut on = set.privs.contains(name);
                if ui.checkbox(&mut on, name.as_str()).changed() {
                    if on {
                        set.privs.insert(name.clone());
                    } else {
                        set.privs.remove(name);
                    }
                }
                if i % 3 == 2 {
                    ui.end_row();
                }
            }
        });
    ui.add_space(6.0);
    ui.checkbox(&mut set.grant_option, "WITH GRANT OPTION");
}

fn sql_preview(ui: &mut Ui, statements: &[String]) {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    let text = if statements.is_empty() {
        format!("-- {}", tr("No changes"))
    } else {
        statements
            .iter()
            .map(|s| format!("{s};"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    egui::Frame::NONE
        .fill(p.editor_bg)
        .stroke(egui::Stroke::new(1.0, p.control_border))
        .corner_radius(6)
        .inner_margin(Margin::same(8))
        .show(ui, |ui| {
            egui::ScrollArea::both()
                .id_salt("user_preview")
                .max_height(140.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    let mut job = sql::highlight(&text, dark, FontId::monospace(12.5));
                    job.wrap.max_width = f32::INFINITY;
                    ui.label(job);
                });
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_masks_and_apply_reveals_the_password() {
        let mut d = UserDialog::new_user(1, Driver::MariaDb);
        d.edit.account.name = " bob ".into();
        d.password = "hunter2".into();
        d.confirm = "hunter2".into();
        let preview = d.statements(false).join("\n");
        assert_eq!(preview, "create user 'bob'@'%' identified by '********'");
        assert_eq!(
            d.validate().unwrap(),
            ["create user 'bob'@'%' identified by 'hunter2'"]
        );
        d.confirm = "other".into();
        assert!(d.validate().is_err(), "passwords differ");
        d.set_error("near 'hunter2' at line 1");
        assert_eq!(d.error.as_deref(), Some("near '********' at line 1"));
    }

    #[test]
    fn plugin_change_needs_a_password() {
        let mut d = UserDialog::edit(1, Driver::MySql, Account::new("a", "%"), false);
        let loaded = UserState {
            account: Account::new("a", "%"),
            plugin: "mysql_native_password".into(),
            ..Default::default()
        };
        d.loaded(loaded, false);
        assert!(d.statements(false).is_empty());
        d.edit.plugin = "caching_sha2_password".into();
        assert!(d.validate().is_err());
        d.edit.plugin = "auth_socket".into();
        assert_eq!(
            d.validate().unwrap(),
            ["alter user 'a'@'%' identified with auth_socket"]
        );
    }

    #[test]
    fn drop_and_resync_after_a_failure() {
        let d = UserDialog::drop(1, Driver::Postgres, Account::new("App", ""));
        assert_eq!(d.validate().unwrap(), ["drop role \"App\""]);
        let mut d = UserDialog::new_user(1, Driver::Postgres);
        d.edit.account.name = "app".into();
        d.edit.member_of.insert("readers".into());
        // CREATE ROLE ran, GRANT failed: the reload becomes the loaded state, the edit stays.
        d.loaded(
            UserState {
                account: Account::new("app", ""),
                ..UserState::new_user(Driver::Postgres)
            },
            true,
        );
        assert_eq!(d.statements(true), ["grant readers to app"]);
    }

    /// Writes previews of the editor to target/user-dialog-snapshots.
    #[test]
    #[ignore = "Requires a GPU; writes user dialog previews to target/user-dialog-snapshots"]
    fn render_user_dialog() {
        use crate::db::users::AccountRow;
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/user-dialog-snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        let list = UserList {
            accounts: ["app", "readers", "writers"]
                .iter()
                .map(|n| AccountRow {
                    account: Account::new(n, ""),
                    can_login: true,
                })
                .collect(),
            global_privileges: [
                "SELECT",
                "INSERT",
                "UPDATE",
                "DELETE",
                "CREATE",
                "DROP",
                "RELOAD",
                "PROCESS",
                "FILE",
                "SUPER",
                "SHOW DATABASES",
                "CREATE USER",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            db_privileges: [
                "SELECT", "INSERT", "UPDATE", "DELETE", "CREATE", "DROP", "INDEX", "ALTER",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        };
        let mut mysql = UserState {
            account: Account::new("demo", "%"),
            plugin: "mysql_native_password".into(),
            ..Default::default()
        };
        mysql.grants.insert(
            PrivTarget::Database("shop".into()),
            PrivSet {
                privs: ["SELECT".into(), "INSERT".into()].into(),
                grant_option: false,
            },
        );
        mysql.global.privs.insert("PROCESS".into());
        let mut pg = UserState {
            account: Account::new("app", ""),
            ..UserState::new_user(Driver::Postgres)
        };
        pg.member_of.insert("readers".into());
        for (name, driver, user, tab) in [
            (
                "mysql-general",
                Driver::MariaDb,
                mysql.clone(),
                Tab::General,
            ),
            ("mysql-server", Driver::MariaDb, mysql.clone(), Tab::Server),
            ("mysql-database", Driver::MariaDb, mysql, Tab::Objects),
            ("pg-general", Driver::Postgres, pg.clone(), Tab::General),
            ("pg-member", Driver::Postgres, pg, Tab::Membership),
        ] {
            let mut d = UserDialog::edit(1, driver, user.account.clone(), false);
            let mut edited = user.clone();
            edited.global.privs.insert("RELOAD".into());
            edited.member_of.insert("writers".into());
            edited.locked = true;
            d.loaded(user, false);
            d.edit = edited;
            d.password = "secret".into();
            d.confirm = "secret".into();
            d.tab = tab;
            let list = list.clone();
            let mut first = true;
            let mut h = egui_kittest::Harness::builder()
                .with_size(vec2(900.0, 860.0))
                .build_ui_state(
                    move |ui, d: &mut UserDialog| {
                        if std::mem::take(&mut first) {
                            theme::install_fonts(ui.ctx());
                            theme::apply(ui.ctx(), false);
                            return;
                        }
                        let uc = UserContext {
                            databases: vec!["shop".into(), "test".into()],
                            objects: vec![PrivTarget::Schema("public".into())],
                            list: Some(&list),
                        };
                        d.show(ui.ctx(), &uc);
                    },
                    d,
                );
            h.run_steps(4);
            h.render()
                .unwrap()
                .save(dir.join(format!("{name}.png")))
                .unwrap();
        }
    }
}
