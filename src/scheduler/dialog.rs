//! SQL task manager. Persistence and service operations run outside the UI thread.

use super::backup::ScheduledBackup;
use super::{Job, Run, Store, database_path, now, service, validate};
use crate::db::ConnectionConfig;
use crate::db::{Driver, backup::BackupRequest};
use crate::i18n::tr;
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver};

mod backup_view;
mod view;

type CatalogReply = (u64, Result<Vec<String>, String>);

enum Action {
    Load,
    Save(Job),
    Delete(i64),
    Pause(i64),
    EnableService,
    DisableService,
}
enum DraftUpdate {
    Keep,
    Replace,
    Clear,
    Pause,
}
enum Navigation {
    New,
    Select(i64),
    Close,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Configuration,
    History,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum IntervalUnit {
    Seconds,
    Minutes,
    Hours,
    Days,
}
impl IntervalUnit {
    const ALL: [Self; 4] = [Self::Seconds, Self::Minutes, Self::Hours, Self::Days];
    fn seconds(self) -> i64 {
        match self {
            Self::Seconds => 1,
            Self::Minutes => 60,
            Self::Hours => 3600,
            Self::Days => 86400,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Seconds => tr("Seconds"),
            Self::Minutes => tr("Minutes"),
            Self::Hours => tr("Hours"),
            Self::Days => tr("Days"),
        }
    }
}
#[derive(Clone, Copy)]
struct Interval {
    value: f64,
    unit: IntervalUnit,
}
impl Interval {
    fn from_seconds(seconds: i64) -> Self {
        let unit = IntervalUnit::ALL.into_iter().rev().find(|unit| seconds >= unit.seconds() && seconds % unit.seconds() == 0).unwrap_or(IntervalUnit::Seconds);
        Self { value: seconds as f64 / unit.seconds() as f64, unit }
    }
    fn seconds(self) -> Option<i64> {
        let seconds = self.value * self.unit.seconds() as f64;
        (seconds.is_finite() && (1.0..=31_536_000.0).contains(&seconds)).then(|| seconds.round() as i64)
    }
    fn change_unit(&mut self, unit: IntervalUnit) {
        self.value = self.value * self.unit.seconds() as f64 / unit.seconds() as f64;
        self.unit = unit;
    }
}

struct Snapshot {
    jobs: Vec<Job>,
    runs: Vec<Run>,
    selected: Option<i64>,
    update: DraftUpdate,
    notice: Option<&'static str>,
    startup_error: Option<String>,
    running: bool,
}

fn save_task(
    store: &mut Store,
    job: &Job,
    now: i64,
    enable: impl FnOnce() -> anyhow::Result<()>,
    status: impl FnOnce() -> anyhow::Result<bool>,
) -> anyhow::Result<(i64, Option<String>, bool)> {
    // Persist first: starting an idle daemon must not run the previous, overdue SQL.
    let id = store.save(job, now)?;
    let mut startup_error = if job.enabled { enable().err().map(|error| format!("{error:#}")) } else { None };
    let running = match status() {
        Ok(running) => running,
        Err(error) => {
            startup_error.get_or_insert_with(|| format!("{error:#}"));
            false
        }
    };
    Ok((id, startup_error, running))
}

pub struct Dialog {
    jobs: Vec<Job>,
    runs: Vec<Run>,
    selected: Option<i64>,
    draft: Job,
    original: Job,
    interval: Interval,
    tab: Tab,
    pending_navigation: Option<Navigation>,
    receiver: Option<Receiver<Result<Snapshot, String>>>,
    message: Option<(String, bool)>,
    running: bool,
    confirm_save: bool,
    confirm_delete: bool,
    cached_backup: Option<(u64, ScheduledBackup)>,
    backup_preview: Option<BackupRequest>,
    catalogs: HashMap<u64, Vec<String>>,
    catalog_receiver: Option<Receiver<CatalogReply>>,
    catalog_error: Option<(u64, String)>,
}

impl Dialog {
    fn empty_job(connections: &[ConnectionConfig]) -> Job {
        Job {
            id: 0,
            name: String::new(),
            connection: connections.iter().find(|c| !c.driver.is_nosql()).map_or(0, |c| c.id),
            database: String::new(),
            sql: String::new(),
            backup: None,
            interval_seconds: 300,
            enabled: true,
            next_run: 0,
        }
    }
    pub fn new(ctx: &egui::Context, connections: &[ConnectionConfig]) -> Self {
        let draft = Self::empty_job(connections);
        let mut dialog = Self {
            jobs: Vec::new(),
            runs: Vec::new(),
            selected: None,
            original: draft.clone(),
            interval: Interval::from_seconds(draft.interval_seconds),
            draft,
            tab: Tab::Configuration,
            pending_navigation: None,
            receiver: None,
            message: None,
            running: false,
            confirm_save: false,
            confirm_delete: false,
            cached_backup: None,
            backup_preview: None,
            catalogs: HashMap::new(),
            catalog_receiver: None,
            catalog_error: None,
        };
        dialog.dispatch(ctx, Action::Load);
        dialog
    }
    fn dirty(&self) -> bool {
        let (a, b) = (&self.draft, &self.original);
        a.name != b.name
            || a.connection != b.connection
            || a.database != b.database
            || a.sql != b.sql
            || a.backup != b.backup
            || a.enabled != b.enabled
            || self.interval.seconds() != Some(b.interval_seconds)
    }
    fn valid(&self, connections: &[ConnectionConfig]) -> bool {
        !self.draft.name.trim().is_empty()
            && self.interval.seconds().is_some()
            && connections.iter().any(|c| {
                c.id == self.draft.connection
                    && match &self.draft.backup {
                        Some(backup) => {
                            !(c.read_only && !BackupRequest::local(c.driver))
                                && (c.driver != Driver::Oracle || self.backup_database(c) == c.database)
                                && backup.validate(c.driver, &self.backup_database(c)).is_ok()
                        }
                        None => !c.driver.is_nosql() && !self.draft.sql.trim().is_empty(),
                    }
            })
    }
    fn set_draft(&mut self, draft: Job) {
        self.interval = Interval::from_seconds(draft.interval_seconds);
        self.original = draft.clone();
        self.draft = draft;
        self.cached_backup = None;
        self.backup_preview = None;
    }

    fn backup_database(&self, connection: &ConnectionConfig) -> String {
        if !self.draft.database.is_empty() {
            self.draft.database.clone()
        } else if connection.driver == Driver::Sqlite {
            "main".into()
        } else {
            connection.database.clone()
        }
    }

    fn select_backup(&mut self, backup: bool, connections: &[ConnectionConfig]) {
        if backup == self.draft.backup.is_some() {
            return;
        }
        if backup {
            let previous_source = self.draft.connection;
            let connection = connections
                .iter()
                .find(|c| c.id == self.draft.connection && !crate::db::backup::BackupMode::for_driver(c.driver).is_empty())
                .or_else(|| connections.iter().find(|c| !crate::db::backup::BackupMode::for_driver(c.driver).is_empty()));
            if let Some(connection) = connection {
                self.draft.connection = connection.id;
                self.draft.backup = Some(match self.cached_backup.take() {
                    Some((id, settings)) if id == connection.id => settings,
                    _ => ScheduledBackup::new(connection.driver),
                });
                if previous_source != connection.id {
                    self.draft.database.clear();
                }
                if self.draft.database.is_empty() {
                    self.draft.database = self.backup_database(connection);
                }
            } else {
                self.draft.connection = 0;
                self.draft.backup = Some(ScheduledBackup::new(Driver::Sqlite));
            }
            if self.draft.name.is_empty() {
                self.draft.name = tr("Database backup").into();
            }
        } else {
            self.cached_backup = self.draft.backup.take().map(|settings| (self.draft.connection, settings));
        }
        self.backup_preview = None;
    }

    fn source_changed(&mut self, previous: u64, connections: &[ConnectionConfig]) {
        if self.draft.backup.is_none() || previous == self.draft.connection {
            return;
        }
        if let Some(connection) = connections.iter().find(|c| c.id == self.draft.connection) {
            if connections.iter().find(|c| c.id == previous).map(|c| c.driver) != Some(connection.driver) {
                self.draft.backup = Some(ScheduledBackup::new(connection.driver));
            }
            self.draft.database = if connection.driver == Driver::Sqlite { "main".into() } else { connection.database.clone() };
        }
        self.cached_backup = None;
        self.backup_preview = None;
    }

    fn load_catalog(&mut self, ctx: &egui::Context, connection: &ConnectionConfig, force: bool) {
        if self.catalog_receiver.is_some() || (!force && self.catalogs.contains_key(&connection.id)) {
            return;
        }
        if matches!(connection.driver, Driver::Sqlite | Driver::Oracle) {
            let names = if connection.driver == Driver::Sqlite {
                vec!["main".into()]
            } else if connection.database.is_empty() {
                Vec::new()
            } else {
                vec![connection.database.clone()]
            };
            self.catalogs.insert(connection.id, names);
            return;
        }
        let cfg = connection.clone();
        let id = cfg.id;
        let (sender, receiver) = mpsc::channel();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new().name("scheduler-databases".into()).spawn(move || {
            let result = (|| -> anyhow::Result<Vec<String>> {
                let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
                runtime.block_on(async {
                    tokio::time::timeout(std::time::Duration::from_secs(30), async {
                        let mut session = crate::db::Session::connect(&cfg).await?;
                        session.list_databases().await
                    })
                    .await?
                })
            })();
            let _ = sender.send((id, result.map_err(|_| tr("Could not load databases. Check the saved connection.").into())));
            ctx.request_repaint();
        });
        match spawned {
            Ok(_) => {
                self.catalog_receiver = Some(receiver);
                self.catalog_error = None;
            }
            Err(_) => self.catalog_error = Some((id, tr("Could not load databases. Check the saved connection.").into())),
        }
    }

    fn open_confirmation(&mut self, connections: &[ConnectionConfig]) {
        self.backup_preview = None;
        if let Some(backup) = &self.draft.backup
            && let Some(connection) = connections.iter().find(|c| c.id == self.draft.connection)
        {
            match backup.request(connection.driver, &self.backup_database(connection)) {
                Ok(request) => self.backup_preview = Some(request),
                Err(error) => {
                    self.message = Some((error.to_string(), true));
                    return;
                }
            }
        }
        self.confirm_save = true;
    }

    fn submit_confirmation(&mut self, connections: &[ConnectionConfig], action: &mut Option<Action>) {
        if let Some(seconds) = self.interval.seconds() {
            let mut job = self.draft.clone();
            job.interval_seconds = seconds;
            if job.backup.is_some() {
                if let Some(connection) = connections.iter().find(|c| c.id == job.connection) {
                    job.database = self.backup_database(connection);
                }
                job.sql.clear();
            }
            *action = Some(Action::Save(job));
        }
        self.confirm_save = false;
    }
    fn apply_navigation(&mut self, connections: &[ConnectionConfig], action: &mut Option<Action>) -> bool {
        match self.pending_navigation.take() {
            Some(Navigation::Close) => return true,
            Some(Navigation::New) => {
                self.selected = None;
                self.runs.clear();
                self.set_draft(Self::empty_job(connections));
                self.tab = Tab::Configuration;
            }
            Some(Navigation::Select(id)) => {
                if let Some(job) = self.jobs.iter().find(|job| job.id == id).cloned() {
                    self.selected = Some(id);
                    self.runs.clear();
                    self.set_draft(job);
                    *action = Some(Action::Load);
                }
            }
            None => {}
        }
        self.message = None;
        false
    }
    fn dispatch(&mut self, ctx: &egui::Context, action: Action) {
        if self.receiver.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        let ctx = ctx.clone();
        let selected = self.selected;
        let spawned = std::thread::Builder::new().name("sql-scheduler-ui".into()).spawn(move || {
            let result = (|| -> anyhow::Result<Snapshot> {
                let path = database_path()?;
                let _control = super::control::lock(&path)?;
                let mut store = Store::open(&path)?;
                let mut startup_error = None;
                let mut saved_running = None;
                let (selected, update, notice) = match action {
                    Action::Load => (selected, DraftUpdate::Keep, None),
                    Action::Save(job) => {
                        validate(&job, now())?;
                        let (id, error, running) = save_task(
                            &mut store,
                            &job,
                            now(),
                            || {
                                service::enable()?;
                                super::wake()
                            },
                            service::is_running,
                        )?;
                        saved_running = Some(running);
                        startup_error = error;
                        (Some(id), DraftUpdate::Replace, Some("Task saved"))
                    }
                    Action::Delete(id) => {
                        store.delete(id)?;
                        (None, DraftUpdate::Clear, Some("Task deleted"))
                    }
                    Action::Pause(id) => {
                        store.pause(id)?;
                        (Some(id), DraftUpdate::Pause, Some("Task paused"))
                    }
                    Action::DisableService => {
                        store.pause_all()?;
                        (selected, DraftUpdate::Pause, Some("All tasks paused; background scheduler stops after active tasks finish"))
                    }
                    Action::EnableService => {
                        if !store.has_enabled()? {
                            anyhow::bail!(tr("Enable a task before starting the background scheduler"));
                        }
                        service::enable()?;
                        (selected, DraftUpdate::Keep, Some("Background scheduler enabled"))
                    }
                };
                let reconcile = (|| -> anyhow::Result<()> {
                    if !store.has_enabled()? {
                        service::unregister()?;
                    }
                    super::wake()
                })();
                if let Err(error) = reconcile {
                    startup_error.get_or_insert_with(|| format!("{error:#}"));
                }
                Ok(Snapshot {
                    jobs: store.jobs()?,
                    runs: if let Some(id) = selected { store.runs(id)? } else { Vec::new() },
                    selected,
                    update,
                    notice,
                    startup_error,
                    running: match saved_running {
                        Some(running) => running,
                        None => service::is_running()?,
                    },
                })
            })();
            let _ = sender.send(result.map_err(|error| format!("{error:#}")));
            ctx.request_repaint();
        });
        match spawned {
            Ok(_) => {
                self.receiver = Some(receiver);
                self.message = None;
            }
            Err(error) => self.message = Some((format!("{}: {error}", tr("Could not start scheduler operation")), true)),
        }
    }
    fn poll(&mut self, connections: &[ConnectionConfig]) {
        let Some(receiver) = &self.receiver else {
            return;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err(tr("Scheduler operation stopped unexpectedly").into()),
        };
        self.receiver = None;
        match result {
            Ok(snapshot) => {
                self.jobs = snapshot.jobs;
                self.runs = snapshot.runs;
                self.selected = snapshot.selected;
                self.running = snapshot.running;
                match snapshot.update {
                    DraftUpdate::Replace => {
                        if let Some(job) = self.jobs.iter().find(|job| Some(job.id) == self.selected).cloned() {
                            self.set_draft(job);
                        }
                    }
                    DraftUpdate::Clear => {
                        self.set_draft(Self::empty_job(connections));
                        self.tab = Tab::Configuration;
                    }
                    DraftUpdate::Pause => {
                        self.original.enabled = false;
                        self.draft.enabled = false;
                    }
                    DraftUpdate::Keep => {}
                }
                self.message = if let Some(error) = snapshot.startup_error {
                    let message = if snapshot.notice == Some("Task saved") {
                        crate::i18n::trf("Task saved; background scheduler error: {error}", &[("error", &error)])
                    } else {
                        format!("{}: {error}", tr(snapshot.notice.unwrap_or("Could not start scheduler operation")))
                    };
                    Some((message, true))
                } else {
                    snapshot.notice.map(|notice| (tr(notice).to_string(), false))
                };
            }
            Err(error) => self.message = Some((error, true)),
        }
    }
    pub fn show(&mut self, ctx: &egui::Context, connections: &[ConnectionConfig]) -> bool {
        if let Some(receiver) = &self.catalog_receiver {
            match receiver.try_recv() {
                Ok((id, result)) => {
                    self.catalog_receiver = None;
                    match result {
                        Ok(mut names) => {
                            names.sort();
                            names.dedup();
                            self.catalogs.insert(id, names);
                        }
                        Err(error) => self.catalog_error = Some((id, error)),
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.catalog_receiver = None;
                    self.catalog_error = Some((self.draft.connection, tr("Could not load databases. Check the saved connection.").into()));
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        self.poll(connections);
        if self.draft.backup.is_some()
            && self.catalog_receiver.is_none()
            && !self.catalogs.contains_key(&self.draft.connection)
            && self.catalog_error.as_ref().is_none_or(|(id, _)| *id != self.draft.connection)
            && let Some(connection) = connections.iter().find(|c| c.id == self.draft.connection)
        {
            self.load_catalog(ctx, connection, false);
        }
        let mut action = None;
        self.render(ctx, connections, &mut action);
        let mut close = false;
        if self.pending_navigation.is_some() {
            if self.dirty() {
                let mut discard = false;
                egui::Modal::new(egui::Id::new("discard_scheduled_sql")).show(ctx, |ui| {
                    ui.heading(tr("Unsaved changes"));
                    ui.label(tr("Discard changes to this task?"));
                    ui.horizontal(|ui| {
                        if crate::ui::theme::primary_button(ui, tr("Keep editing")).clicked() {
                            self.pending_navigation = None;
                        }
                        if ui.button(tr("Discard changes")).clicked() {
                            discard = true;
                        }
                    });
                });
                if discard {
                    close = self.apply_navigation(connections, &mut action);
                }
            } else {
                close = self.apply_navigation(connections, &mut action);
            }
        }
        if self.confirm_save && self.draft.backup.is_some() {
            self.confirm_backup(ctx, connections, &mut action);
        } else if self.confirm_save {
            egui::Modal::new(egui::Id::new("confirm_scheduled_sql")).show(ctx, |ui| {
                ui.set_max_width(620.0_f32.min(ctx.content_rect().width() - 64.0));
                ui.heading(tr("Confirm task"));
                ui.label(tr("This SQL will run automatically and may change data."));
                egui::ScrollArea::both().max_height(220.0).show(ui, |ui| {
                    ui.label(egui::RichText::new(&self.draft.sql).monospace());
                });
                ui.horizontal(|ui| {
                    if crate::ui::theme::primary_button(ui, tr("Save task")).clicked() {
                        self.submit_confirmation(connections, &mut action);
                    }
                    if ui.button(tr("Keep editing")).clicked() {
                        self.confirm_save = false;
                    }
                });
            });
        }
        if self.confirm_delete {
            egui::Modal::new(egui::Id::new("delete_scheduled_sql")).show(ctx, |ui| {
                ui.heading(tr("Delete task"));
                ui.label(tr("Delete this task and its history?"));
                ui.horizontal(|ui| {
                    if ui.button(tr("Confirm deletion")).clicked() {
                        self.confirm_delete = false;
                        action = Some(Action::Delete(self.draft.id));
                    }
                    if ui.button(tr("Cancel")).clicked() {
                        self.confirm_delete = false;
                    }
                });
            });
        }
        if let Some(action) = action {
            self.dispatch(ctx, action);
        }
        close
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Driver;
    use egui_kittest::kittest::Queryable;

    #[test]
    fn turn_off_is_accessible_without_discarding_the_edited_task() {
        let connection = ConnectionConfig::new(1, Driver::Sqlite);
        let mut dialog = fixture();
        dialog.running = true;
        let original_sql = dialog.draft.sql.clone();
        let mut first = true;
        let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(400.0, 300.0)).build_ui_state(
            move |ui, state: &mut (Dialog, Option<Action>)| {
                if std::mem::take(&mut first) {
                    crate::ui::theme::install_fonts(ui.ctx());
                    return;
                }
                state.0.render(ui.ctx(), std::slice::from_ref(&connection), &mut state.1);
            },
            (dialog, None),
        );
        h.run_steps(3);
        let bounds = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 300.0));
        assert!(bounds.contains_rect(h.get_by_label("Turn off").rect()));
        h.get_by_label("Turn off").click();
        h.run_steps(3);
        assert!(matches!(h.state().1, Some(Action::DisableService)));
        assert_eq!(h.state().0.draft.sql, original_sql);
    }

    #[test]
    fn save_persists_the_new_schedule_before_starting_the_service() {
        let mut store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let mut task = fixture().draft;
        task.id = store.save(&task, 100).unwrap();
        task.sql = "SELECT 42".into();
        let saved = save_task(&mut store, &task, 200, || anyhow::bail!("service unavailable"), || Ok(false)).unwrap();
        assert_eq!(saved.0, task.id);
        assert!(saved.1.unwrap().contains("service unavailable"));
        let jobs = store.jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].sql, "SELECT 42");
        assert_eq!(jobs[0].next_run, 260);
    }

    #[test]
    fn rejected_save_does_not_start_the_service() {
        let mut store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let mut task = fixture().draft;
        task.id = store.save(&task, 100).unwrap();
        store.claim_due(160).unwrap().unwrap();
        let started = std::cell::Cell::new(false);
        assert!(
            save_task(
                &mut store,
                &task,
                200,
                || {
                    started.set(true);
                    Ok(())
                },
                || Ok(false)
            )
            .is_err()
        );
        assert!(!started.get());
    }

    #[test]
    fn service_observes_the_saved_query_instead_of_the_overdue_query() {
        let path = std::env::temp_dir().join(format!("datacraft-save-order-{}.sqlite", uuid::Uuid::new_v4()));
        let mut store = Store::open(&path).unwrap();
        let mut task = fixture().draft;
        task.id = store.save(&task, 100).unwrap();
        task.sql = "SELECT 42".into();
        let result = save_task(
            &mut store,
            &task,
            200,
            || {
                let mut service_store = Store::open(&path)?;
                assert_eq!(service_store.jobs()?[0].sql, "SELECT 42");
                assert!(service_store.claim_due(200)?.is_none());
                Ok(())
            },
            || Ok(true),
        )
        .unwrap();
        assert!(result.1.is_none());
        drop(store);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn failed_startup_keeps_the_saved_id_for_retry() {
        let mut store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let connections = [ConnectionConfig::new(1, Driver::Sqlite)];
        let mut dialog = fixture();
        let (id, startup_error, running) = save_task(&mut store, &dialog.draft, 100, || anyhow::bail!("unavailable"), || Ok(false)).unwrap();
        let (sender, receiver) = mpsc::channel();
        dialog.receiver = Some(receiver);
        sender
            .send(Ok(Snapshot {
                jobs: store.jobs().unwrap(),
                runs: Vec::new(),
                selected: Some(id),
                update: DraftUpdate::Replace,
                notice: Some("Task saved"),
                startup_error,
                running,
            }))
            .unwrap();
        dialog.poll(&connections);
        assert_eq!(dialog.draft.id, id);
        assert!(!dialog.dirty());
        assert!(dialog.message.as_ref().is_some_and(|(message, error)| *error && message.contains("Task saved")));
        save_task(&mut store, &dialog.draft, 200, || Ok(()), || Ok(true)).unwrap();
        assert_eq!(store.jobs().unwrap().len(), 1);
    }

    #[test]
    fn failed_status_probe_keeps_the_saved_task() {
        for enabled in [false, true] {
            let mut store = Store::open(std::path::Path::new(":memory:")).unwrap();
            let mut task = fixture().draft;
            task.enabled = enabled;
            let (id, warning, running) = save_task(&mut store, &task, 100, || Ok(()), || anyhow::bail!("lock file unavailable")).unwrap();
            assert_eq!(store.jobs().unwrap()[0].id, id);
            assert!(warning.unwrap().contains("lock file unavailable"));
            assert!(!running);
        }
    }

    #[test]
    fn backup_database_picker_accepts_manual_input_and_closes_on_selection() {
        use egui::accesskit::Role;
        let connection = ConnectionConfig::new(1, Driver::Postgres);
        let mut dialog = backup_fixture();
        dialog.draft.database.clear();
        dialog.catalogs.insert(1, vec!["listed".into()]);
        let mut first = true;
        let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(600.0, 400.0)).build_ui_state(
            move |ui, dialog: &mut Dialog| {
                if std::mem::take(&mut first) {
                    crate::ui::theme::install_fonts(ui.ctx());
                    return;
                }
                dialog.database_picker(ui, &connection);
            },
            dialog,
        );
        h.run_steps(3);
        h.get_by_role(Role::ComboBox).click();
        h.run_steps(3);
        h.get_by_role(Role::TextInput).click();
        h.run_steps(3);
        assert!(h.query_by_role(Role::TextInput).is_some(), "Clicking the database editor must keep the picker open");
        h.get_by_role(Role::TextInput).type_text(" unlisted database ");
        h.run_steps(3);
        assert_eq!(h.state().draft.database, " unlisted database ");
        h.get_by_label("listed").click();
        h.run_steps(3);
        assert_eq!(h.state().draft.database, "listed");
        assert!(h.query_by_role(Role::TextInput).is_none());
    }

    #[test]
    fn escape_closes_a_picker_without_closing_the_scheduler() {
        let connection = ConnectionConfig::new(1, Driver::Sqlite);
        let mut dialog = fixture();
        dialog.set_draft(dialog.draft.clone());
        let mut first = true;
        let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(1000.0, 760.0)).build_ui_state(
            move |ui, dialog: &mut Dialog| {
                if std::mem::take(&mut first) {
                    crate::ui::theme::install_fonts(ui.ctx());
                    return;
                }
                dialog.render(ui.ctx(), std::slice::from_ref(&connection), &mut None);
            },
            dialog,
        );
        h.run_steps(3);
        h.get_by_value("SQLite 1").click();
        h.run_steps(3);
        h.key_press(egui::Key::Escape);
        h.run_steps(3);
        assert!(h.state().pending_navigation.is_none());
        h.key_press(egui::Key::Escape);
        h.run_steps(3);
        assert!(matches!(h.state().pending_navigation, Some(Navigation::Close)));
    }

    fn backup_fixture() -> Dialog {
        let mut dialog = fixture();
        dialog.draft.sql.clear();
        dialog.draft.database = "main".into();
        let mut backup = crate::scheduler::backup::ScheduledBackup::new(Driver::Sqlite);
        backup.folder = std::env::temp_dir().to_string_lossy().into_owned();
        dialog.draft.backup = Some(backup);
        dialog
    }

    #[test]
    fn backup_task_does_not_require_sql() {
        let dialog = backup_fixture();
        assert!(dialog.valid(&[ConnectionConfig::new(1, Driver::Sqlite)]));
    }

    #[test]
    fn backup_confirmation_preserves_exact_database_identifiers() {
        let mut connection = ConnectionConfig::new(1, Driver::Postgres);
        connection.database = " app ".into();
        let mut dialog = backup_fixture();
        dialog.draft.backup.as_mut().unwrap().utility = "pg_dump".into();
        dialog.draft.database = " shop ".into();
        dialog.open_confirmation(std::slice::from_ref(&connection));
        assert_eq!(dialog.backup_preview.as_ref().unwrap().db, " shop ");
        dialog.draft.database.clear();
        dialog.open_confirmation(std::slice::from_ref(&connection));
        assert_eq!(dialog.backup_preview.as_ref().unwrap().db, " app ");
    }

    #[test]
    fn backup_confirmation_keeps_actions_visible_with_long_destinations() {
        let connection = ConnectionConfig::new(1, Driver::Sqlite);
        let mut dialog = backup_fixture();
        dialog.draft.backup.as_mut().unwrap().folder = std::env::temp_dir().join("a".repeat(1500)).to_string_lossy().into_owned();
        dialog.open_confirmation(std::slice::from_ref(&connection));
        let size = egui::vec2(400.0, 300.0);
        let mut first = true;
        let mut harness = egui_kittest::Harness::builder().with_size(size).build_ui_state(
            move |ui, dialog: &mut Dialog| {
                if std::mem::take(&mut first) {
                    crate::ui::theme::install_fonts(ui.ctx());
                    return;
                }
                dialog.show(ui.ctx(), std::slice::from_ref(&connection));
            },
            dialog,
        );
        harness.run_steps(3);
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
        assert!(screen.contains_rect(harness.get_by_label("Keep editing").rect()));
        assert!(screen.contains_rect(harness.get_by_label("Confirm task").rect()));
    }

    #[test]
    fn backup_confirmation_keeps_actions_visible_with_server_sql_expanded() {
        let mut connection = ConnectionConfig::new(1, Driver::Oracle);
        connection.database = "XE".into();
        let mut dialog = backup_fixture();
        dialog.draft.database = "XE".into();
        let mut backup = ScheduledBackup::new(Driver::Oracle);
        backup.directory = "BACKUP_DIR".into();
        dialog.draft.backup = Some(backup);
        dialog.open_confirmation(std::slice::from_ref(&connection));
        let size = egui::vec2(400.0, 300.0);
        let mut first = true;
        let mut harness = egui_kittest::Harness::builder().with_size(size).build_ui_state(
            move |ui, dialog: &mut Dialog| {
                if std::mem::take(&mut first) {
                    crate::ui::theme::install_fonts(ui.ctx());
                    return;
                }
                dialog.show(ui.ctx(), std::slice::from_ref(&connection));
            },
            dialog,
        );
        harness.run_steps(3);
        harness.get_by_label("SQL preview").scroll_to_me();
        harness.run_steps(4);
        harness.get_by_label("SQL preview").click();
        harness.run_steps(4);
        assert!(harness.query_by_label_contains("DBMS_DATAPUMP").is_some());
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
        assert!(screen.contains_rect(harness.get_by_label("Keep editing").rect()));
        assert!(screen.contains_rect(harness.get_by_label("Confirm task").rect()));
    }

    #[test]
    fn backup_changes_are_dirty() {
        let mut dialog = backup_fixture();
        dialog.set_draft(dialog.draft.clone());
        assert!(!dialog.dirty());
        dialog.draft.backup.as_mut().unwrap().compress = true;
        assert!(dialog.dirty());
    }

    #[test]
    fn switching_task_type_keeps_sql_and_backup_settings_until_saved() {
        let connections = [ConnectionConfig::new(1, Driver::Sqlite)];
        let mut dialog = fixture();
        let sql = dialog.draft.sql.clone();
        dialog.select_backup(true, &connections);
        dialog.draft.backup.as_mut().unwrap().folder = std::env::temp_dir().to_string_lossy().into_owned();
        let settings = dialog.draft.backup.clone();
        dialog.select_backup(false, &connections);
        assert_eq!(dialog.draft.sql, sql);
        dialog.select_backup(true, &connections);
        assert_eq!(dialog.draft.backup, settings);
        assert_eq!(dialog.draft.sql, sql);
    }

    #[test]
    fn backup_confirmation_does_not_require_a_sql_statement() {
        let connection = ConnectionConfig::new(1, Driver::Sqlite);
        let mut dialog = backup_fixture();
        dialog.open_confirmation(std::slice::from_ref(&connection));
        assert!(dialog.backup_preview.is_some());
        let mut first = true;
        let mut harness = egui_kittest::Harness::builder().with_size(egui::vec2(1000.0, 760.0)).build_ui_state(
            move |ui, dialog: &mut Dialog| {
                if std::mem::take(&mut first) {
                    crate::ui::theme::install_fonts(ui.ctx());
                    return;
                }
                dialog.show(ui.ctx(), std::slice::from_ref(&connection));
            },
            dialog,
        );
        harness.run_steps(3);
        assert!(harness.query_by_label("A full backup will be created automatically at the selected interval.").is_some());
        assert!(harness.query_by_label("This SQL will run automatically and may change data.").is_none());
    }

    #[test]
    fn backup_form_shows_a_destination_folder() {
        let connection = ConnectionConfig::new(1, Driver::Sqlite);
        let mut first = true;
        let mut harness = egui_kittest::Harness::builder().with_size(egui::vec2(1000.0, 760.0)).build_ui_state(
            move |ui, dialog: &mut Dialog| {
                if std::mem::take(&mut first) {
                    crate::ui::theme::install_fonts(ui.ctx());
                    return;
                }
                dialog.show(ui.ctx(), std::slice::from_ref(&connection));
            },
            backup_fixture(),
        );
        harness.run_steps(3);
        assert!(harness.query_by_label("Destination folder").is_some());
    }

    fn fixture() -> Dialog {
        Dialog {
            jobs: Vec::new(),
            runs: Vec::new(),
            selected: None,
            draft: Job {
                id: 0,
                name: "task".into(),
                connection: 1,
                database: String::new(),
                sql: "DELETE FROM records".into(),
                backup: None,
                interval_seconds: 60,
                enabled: true,
                next_run: 0,
            },
            receiver: None,
            message: None,
            original: Dialog::empty_job(&[ConnectionConfig::new(1, Driver::Sqlite)]),
            interval: Interval::from_seconds(60),
            tab: Tab::Configuration,
            pending_navigation: None,
            running: false,
            confirm_save: false,
            confirm_delete: false,
            cached_backup: None,
            backup_preview: None,
            catalogs: HashMap::new(),
            catalog_receiver: None,
            catalog_error: None,
        }
    }

    #[test]
    fn footer_actions_stay_visible_on_small_viewports() {
        for size in [egui::vec2(400.0, 300.0), egui::vec2(780.0, 560.0), egui::vec2(1000.0, 700.0)] {
            let connection = ConnectionConfig::new(1, Driver::Sqlite);
            let mut first = true;
            let mut harness = egui_kittest::Harness::builder().with_size(size).build_ui_state(
                move |ui, dialog: &mut Dialog| {
                    if std::mem::take(&mut first) {
                        crate::ui::theme::install_fonts(ui.ctx());
                        return;
                    }
                    dialog.show(ui.ctx(), std::slice::from_ref(&connection));
                },
                fixture(),
            );
            harness.run_steps(3);
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
            for label in ["Save task", "Cancel"] {
                assert!(screen.contains_rect(harness.get_by_label(label).rect()), "{label} is outside {size:?}");
            }
        }
    }

    #[test]
    fn saving_a_task_requires_confirmation() {
        let connection = ConnectionConfig::new(1, Driver::Sqlite);
        let dialog = fixture();
        let mut first = true;
        let mut harness = egui_kittest::Harness::builder().with_size(egui::vec2(1000.0, 900.0)).build_ui_state(
            move |ui, dialog: &mut Dialog| {
                if std::mem::take(&mut first) {
                    crate::ui::theme::install_fonts(ui.ctx());
                    return;
                }
                dialog.show(ui.ctx(), std::slice::from_ref(&connection));
            },
            dialog,
        );
        harness.run_steps(2);
        harness.get_by_label("Save task").click();
        harness.run_steps(2);
        assert!(harness.query_by_label("This SQL will run automatically and may change data.").is_some());
        assert!(harness.state().receiver.is_none());
        harness.get_by_label("Keep editing").click();
        harness.run_steps(3);
        assert!(!harness.state().confirm_save);
    }

    #[test]
    fn interval_units_roundtrip() {
        let mut interval = Interval::from_seconds(300);
        assert_eq!(interval.unit, IntervalUnit::Minutes);
        assert_eq!(interval.value, 5.0);
        interval.change_unit(IntervalUnit::Hours);
        assert_eq!(interval.seconds(), Some(300));
        interval.value = f64::INFINITY;
        assert_eq!(interval.seconds(), None);
    }

    #[test]
    fn navigation_preserves_unsaved_sql() {
        let connection = ConnectionConfig::new(1, Driver::Sqlite);
        let mut first = true;
        let mut harness = egui_kittest::Harness::builder().with_size(egui::vec2(1000.0, 760.0)).build_ui_state(
            move |ui, dialog: &mut Dialog| {
                if std::mem::take(&mut first) {
                    crate::ui::theme::install_fonts(ui.ctx());
                    return;
                }
                dialog.show(ui.ctx(), std::slice::from_ref(&connection));
            },
            fixture(),
        );
        harness.run_steps(3);
        harness.get_by_role_and_label(egui::accesskit::Role::Button, "New task").click();
        harness.run_steps(3);
        assert!(harness.query_by_label("Discard changes to this task?").is_some());
        assert_eq!(harness.state().draft.sql, "DELETE FROM records");
        harness.get_by_label("Keep editing").click();
        harness.run_steps(3);
        assert!(harness.state().pending_navigation.is_none());
        assert_eq!(harness.state().draft.sql, "DELETE FROM records");
    }

    #[test]
    fn refresh_and_pause_keep_the_edited_draft() {
        let connections = [ConnectionConfig::new(1, Driver::Sqlite)];
        let mut dialog = fixture();
        let edited = dialog.draft.sql.clone();
        for update in [DraftUpdate::Keep, DraftUpdate::Pause] {
            let (sender, receiver) = mpsc::channel();
            dialog.receiver = Some(receiver);
            sender.send(Ok(Snapshot { jobs: Vec::new(), runs: Vec::new(), selected: None, update, notice: None, startup_error: None, running: true })).unwrap();
            dialog.poll(&connections);
            assert_eq!(dialog.draft.sql, edited);
            assert!(dialog.dirty());
        }
        assert!(!dialog.draft.enabled);
    }

    #[test]
    fn confirmation_actions_fit_with_long_sql_on_small_viewports() {
        let connection = ConnectionConfig::new(1, Driver::Sqlite);
        let mut dialog = fixture();
        dialog.draft.sql = "SELECT 1;\n".repeat(100);
        dialog.confirm_save = true;
        let size = egui::vec2(400.0, 300.0);
        let mut first = true;
        let mut harness = egui_kittest::Harness::builder().with_size(size).build_ui_state(
            move |ui, dialog: &mut Dialog| {
                if std::mem::take(&mut first) {
                    crate::ui::theme::install_fonts(ui.ctx());
                    return;
                }
                dialog.show(ui.ctx(), std::slice::from_ref(&connection));
            },
            dialog,
        );
        harness.run_steps(3);
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
        assert!(screen.contains_rect(harness.get_by_label("Keep editing").rect()));
        assert!(screen.contains_rect(harness.get_by_label("Confirm task").rect()));
    }

    #[test]
    fn long_task_name_keeps_the_compact_new_button_visible() {
        let connection = ConnectionConfig::new(1, Driver::Sqlite);
        let mut dialog = fixture();
        dialog.draft.id = 1;
        dialog.draft.name = "Very long task name ".repeat(30);
        dialog.jobs = vec![dialog.draft.clone()];
        dialog.set_draft(dialog.draft.clone());
        let size = egui::vec2(600.0, 560.0);
        let mut first = true;
        let mut harness = egui_kittest::Harness::builder().with_size(size).build_ui_state(
            move |ui, dialog: &mut Dialog| {
                if std::mem::take(&mut first) {
                    crate::ui::theme::install_fonts(ui.ctx());
                    return;
                }
                dialog.show(ui.ctx(), std::slice::from_ref(&connection));
            },
            dialog,
        );
        harness.run_steps(3);
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
        assert!(screen.contains_rect(harness.get_by_role_and_label(egui::accesskit::Role::Button, "New task").rect()));
    }

    #[test]
    #[ignore = "Requires a GPU; renders backup task controls on synthetic state without connecting"]
    fn render_backup_tasks() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/scheduler-ui-snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        for driver in [Driver::Sqlite, Driver::Postgres, Driver::MsSql, Driver::Oracle, Driver::Snowflake] {
            for dark in [false, true] {
                let mut connection = ConnectionConfig::new(1, driver);
                connection.database = if driver == Driver::Sqlite { "main".into() } else { "shop".into() };
                let mut dialog = fixture();
                dialog.draft.sql.clear();
                dialog.draft.name = "Daily database backup".into();
                dialog.draft.database = connection.database.clone();
                let mut backup = ScheduledBackup::new(driver);
                backup.folder = if driver == Driver::MsSql {
                    r"D:\DatabaseBackups".into()
                } else {
                    std::env::temp_dir().join("DataCraft Backups").to_string_lossy().into_owned()
                };
                if driver == Driver::Oracle {
                    backup.directory = "BACKUP_DIR".into();
                }
                dialog.draft.backup = Some(backup);
                dialog.interval = Interval::from_seconds(86400);
                dialog.catalogs.insert(1, vec![connection.database.clone()]);
                let mut first = true;
                let mut harness = egui_kittest::Harness::builder().with_size(egui::vec2(1100.0, 800.0)).build_ui_state(
                    move |ui, dialog: &mut Dialog| {
                        if std::mem::take(&mut first) {
                            crate::ui::theme::install_fonts(ui.ctx());
                            crate::ui::theme::apply(ui.ctx(), dark);
                            return;
                        }
                        dialog.show(ui.ctx(), std::slice::from_ref(&connection));
                    },
                    dialog,
                );
                harness.run_steps(4);
                harness
                    .render()
                    .unwrap()
                    .save(dir.join(format!("backup-{}-{}.png", driver.label().replace('/', "-"), if dark { "dark" } else { "light" })))
                    .unwrap();
            }
        }
    }

    #[test]
    #[ignore = "Requires a GPU; renders the scheduler on synthetic data without running SQL"]
    fn render_scheduler_manager() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/scheduler-ui-snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        for (dark, history) in [(false, false), (true, false), (false, true)] {
            let connection = ConnectionConfig::new(1, Driver::Sqlite);
            let mut dialog = fixture();
            dialog.draft.id = 1;
            dialog.draft.name = "Refresh daily sales".into();
            dialog.draft.sql = "INSERT INTO daily_sales (day, total)\nSELECT date(created_at), SUM(amount)\nFROM orders\nGROUP BY date(created_at);".into();
            dialog.draft.interval_seconds = 300;
            dialog.draft.next_run = 1793448300;
            dialog.set_draft(dialog.draft.clone());
            dialog.selected = Some(1);
            dialog.jobs = vec![dialog.draft.clone(), Job { id: 2, name: "Archive old events".into(), enabled: false, ..dialog.draft.clone() }];
            dialog.running = true;
            dialog.runs = vec![
                Run { id: 3, started: 1793448000, finished: Some(1793448002), status: "succeeded".into(), message: String::new() },
                Run {
                    id: 2,
                    started: 1793447700,
                    finished: Some(1793447701),
                    status: "failed".into(),
                    message: "The database was unavailable. Check the saved data source and try again.".into(),
                },
            ];
            if history {
                dialog.tab = Tab::History;
            }
            let mut first = true;
            let mut harness = egui_kittest::Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_ui_state(
                move |ui, dialog: &mut Dialog| {
                    if std::mem::take(&mut first) {
                        crate::ui::theme::install_fonts(ui.ctx());
                        crate::ui::theme::apply(ui.ctx(), dark);
                        return;
                    }
                    dialog.show(ui.ctx(), std::slice::from_ref(&connection));
                },
                dialog,
            );
            harness.run_steps(4);
            harness
                .render()
                .unwrap()
                .save(dir.join(format!("scheduler-{}-{}.png", if dark { "dark" } else { "light" }, if history { "history" } else { "configuration" })))
                .unwrap();
        }
    }
}
