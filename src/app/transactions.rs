//! Manual transactions of SQL consoles: the app side of `Request::EndTransaction` and the
//! "Commit / Roll Back / Cancel" confirmation that guards closing, switching, disconnecting,
//! editing data sources and quitting.

use super::{ConnStatus, DataCraftApp, now};
use crate::db::ConnectionConfig;
use crate::i18n::{tr, trf};
use crate::ui::console::{CloseResults, ResultTab};
use crate::ui::results::{self, OutputEntry};
use crate::ui::theme;
use crate::worker::{ConnId, Request, TxInfo, TxNote};
use egui::{Key, RichText};
use std::time::{Duration, Instant};

/// Longest wait for the rollbacks chosen in the confirmation before going ahead anyway (closing
/// a session rolls back too, but over HTTP (Snowflake) only once the app still runs).
const ROLLBACK_WAIT: Duration = Duration::from_secs(5);

/// What waits for open Manual transactions to end.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum TxThen {
    Close(u64),
    CloseTabs(u64, CloseResults),
    CloseAll,
    /// Back to Auto mode.
    Auto(u64),
    /// Another connection / database for the console.
    Source(u64, Option<ConnId>, Option<String>),
    Disconnect(ConnId),
    /// Data sources saved (reconnecting them) or deleted, from the connection dialog or Delete.
    Connections { save: Vec<ConnectionConfig>, delete: Vec<ConnId>, close_dialog: bool },
    Quit,
}

/// "Commit / Roll Back / Cancel" before `then`.
pub(super) struct TxPrompt {
    /// Open transactions the question is about.
    pub consoles: Vec<u64>,
    /// Consoles whose Commit or Roll Back is in flight: `then` waits for them.
    pub ending: Vec<u64>,
    pub then: TxThen,
    /// The answer: Some(true) Commit, Some(false) Roll Back.
    pub chosen: Option<bool>,
    /// Chosen rollbacks are waited for until then at most.
    pub deadline: Option<Instant>,
}

impl DataCraftApp {
    /// true if `then` can go ahead now; with open Manual transactions among `consoles` (or a
    /// Commit / Roll Back of theirs in flight), asks to commit or roll them back first, or waits,
    /// and goes ahead afterwards.
    pub(super) fn tx_guard(&mut self, consoles: Vec<u64>, then: TxThen) -> bool {
        let of = |id: &u64| self.consoles.iter().find(|c| c.id == *id);
        let open: Vec<u64> = consoles.iter().copied().filter(|id| of(id).is_some_and(|c| c.may_hold_tx() && !c.ending_tx)).collect();
        let ending: Vec<u64> = consoles.iter().copied().filter(|id| of(id).is_some_and(|c| c.ending_tx)).collect();
        if open.is_empty() && ending.is_empty() {
            return true;
        }
        if self.tx_prompt.is_none() {
            self.tx_prompt = Some(TxPrompt { consoles: open, ending, then, chosen: None, deadline: None });
        }
        false
    }

    /// Ids of the consoles on `conn`.
    pub(super) fn consoles_on(&self, conn: ConnId) -> Vec<u64> {
        self.consoles.iter().filter(|c| c.conn == Some(conn)).map(|c| c.id).collect()
    }

    fn tx_then(&mut self, ctx: &egui::Context, then: TxThen) {
        match then {
            TxThen::Close(id) => {
                if let Some(i) = self.consoles.iter().position(|c| c.id == id) {
                    self.close_console(i);
                }
            }
            TxThen::CloseTabs(id, which) => {
                if self.guard(super::Discard::CloseTabs(id, which)) {
                    self.close_tabs_now(id, which);
                }
            }
            TxThen::CloseAll => {
                if self.guard(super::Discard::CloseAll) {
                    self.discard_and_do(super::Discard::CloseAll);
                }
            }
            TxThen::Auto(id) => self.set_manual(id, false),
            TxThen::Source(id, conn, db) => self.set_console_source(id, conn, db),
            TxThen::Disconnect(conn) => self.disconnect(conn),
            TxThen::Connections { save, delete, close_dialog } => self.apply_connections(save, delete, close_dialog),
            TxThen::Quit => {
                self.quit_confirmed = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    /// Saves and deletes data sources (from the connection dialog), asking first about the open
    /// transactions of the connections that close.
    pub(super) fn connections_changed(&mut self, save: Vec<ConnectionConfig>, delete: Vec<ConnId>, close_dialog: bool) {
        let reconnected = save.iter().filter(|cfg| self.conn(cfg.id).is_some_and(|c| c.status != ConnStatus::Disconnected)).map(|cfg| cfg.id);
        let closing: Vec<u64> = delete.iter().copied().chain(reconnected).flat_map(|id| self.consoles_on(id)).collect();
        if self.tx_guard(closing, TxThen::Connections { save: save.clone(), delete: delete.clone(), close_dialog }) {
            self.apply_connections(save, delete, close_dialog);
        }
    }

    fn apply_connections(&mut self, save: Vec<ConnectionConfig>, delete: Vec<ConnId>, close_dialog: bool) {
        if close_dialog {
            self.dialog = None;
        }
        for id in delete {
            self.delete_connection(id);
        }
        for cfg in save {
            self.save_connection(cfg);
        }
    }

    /// Auto or Manual mode for a console (any open transaction already dealt with).
    pub(super) fn set_manual(&mut self, id: u64, manual: bool) {
        let Some(c) = self.consoles.iter_mut().find(|c| c.id == id) else { return };
        if c.manual_tx && !manual {
            self.worker.send(Request::ReleaseConsole { console: id });
        }
        c.manual_tx = manual;
        c.tx = None;
        // Grids are editable in Auto mode only (changes are saved on the shared session).
        for v in c.results.iter_mut().filter(|v| !v.is_dirty()) {
            v.edit_checked = false;
        }
        self.attach_edit_targets(id);
    }

    /// A Manual console moves to another connection / database: its own session is released
    /// and the next statement opens one there (Manual mode ends on engines without transactions).
    pub(super) fn set_console_source(&mut self, id: u64, conn: Option<ConnId>, db: Option<String>) {
        let target = conn.and_then(|c| self.conn(c)).map(|c| (c.cfg.driver.has_transactions(), c.status.clone()));
        let Some(c) = self.consoles.iter_mut().find(|c| c.id == id) else { return };
        if c.manual_tx {
            self.worker.send(Request::ReleaseConsole { console: id });
        }
        c.conn = conn;
        c.db = db;
        c.pending = None;
        c.run_started = None;
        c.tx = None;
        c.manual_tx &= target.as_ref().is_none_or(|(tx, _)| *tx);
        if let (Some(conn), Some((_, ConnStatus::Disconnected | ConnStatus::Error(_)))) = (conn, target) {
            self.connect(conn);
        }
    }

    /// Sends Commit (true) or Roll Back for console `idx`; false if it has nothing to end.
    pub(super) fn end_transaction(&mut self, idx: usize, commit: bool) -> bool {
        let Some(c) = self.consoles.get_mut(idx).filter(|c| c.tx.is_some() && !c.ending_tx) else { return false };
        c.ending_tx = true;
        if !c.running {
            c.run_started = Some(Instant::now());
        }
        self.worker.send(Request::EndTransaction { console: c.id, commit });
        true
    }

    /// The data source was disconnected (by the user, or deleted / edited): its consoles'
    /// transactions are gone (the server rolls them back) and their own sessions are released.
    /// A lost shared session doesn't come here: Manual sessions are connections of their own.
    pub(super) fn drop_transactions(&mut self, conn: ConnId) {
        for c in self.consoles.iter_mut().filter(|c| c.conn == Some(conn) && c.manual_tx) {
            self.worker.send(Request::ReleaseConsole { console: c.id });
            // Replies of a run or end still in flight describe the released session.
            let in_flight = c.running || c.ending_tx;
            c.tx_released |= in_flight;
            c.ending_tx = false;
            if c.tx.take().is_some() || in_flight {
                c.log.push(OutputEntry { time: now(), statement: String::new(), message: tr("The connection closed, so the open transaction was rolled back").into(), is_error: true });
                c.result_tab = ResultTab::Output;
            }
        }
        let consoles = &self.consoles;
        if let Some(p) = self.tx_prompt.as_mut() {
            p.consoles.retain(|id| consoles.iter().any(|c| c.id == *id && c.may_hold_tx()));
            p.ending.retain(|id| consoles.iter().any(|c| c.id == *id && c.ending_tx));
        }
    }

    /// The transaction after a Manual run, and why it ended if it ended by itself.
    pub(super) fn transaction_reported(&mut self, console: u64, tx: Option<TxInfo>, note: Option<TxNote>) {
        // A console closed (or back in Auto mode) while it ran: its session goes too.
        let Some(c) = self.consoles.iter_mut().find(|c| c.id == console && c.manual_tx) else {
            self.worker.send(Request::ReleaseConsole { console });
            return;
        };
        // Its session was released while this ran: the transaction was already reported as
        // rolled back, and the worker may have kept a session opened meanwhile.
        if std::mem::take(&mut c.tx_released) {
            c.tx = None;
            self.worker.send(Request::ReleaseConsole { console });
            return;
        }
        c.tx = tx;
        let message = match note {
            Some(TxNote::Lost) => Some((tr("The session closed, so the open transaction was rolled back"), true)),
            Some(TxNote::ImplicitCommit) => Some((tr("The statement committed the open transaction implicitly; the next statements run in a new one"), false)),
            Some(TxNote::RolledBack) => Some((tr("The server rolled back the transaction after the error"), true)),
            Some(TxNote::MaybeCommitted) => Some((tr("The statement failed, but the server may already have committed the open transaction before running it"), true)),
            None => None,
        };
        if let Some((message, is_error)) = message {
            c.log.push(OutputEntry { time: now(), statement: String::new(), message: message.into(), is_error });
        }
    }

    pub(super) fn transaction_ended(&mut self, console: u64, commit: bool, result: Result<(), String>, tx: Option<TxInfo>, elapsed: Duration) {
        if let Some(c) = self.consoles.iter_mut().find(|c| c.id == console) {
            c.ending_tx = false;
            if !c.running {
                c.run_started = None;
            }
            c.tx = if std::mem::take(&mut c.tx_released) { None } else { tx };
            let time = results::format_duration(elapsed);
            let (message, is_error) = match &result {
                Ok(()) if commit => (trf("Transaction committed in {time}", &[("time", &time)]), false),
                Ok(()) => (trf("Transaction rolled back in {time}", &[("time", &time)]), false),
                Err(e) => (e.clone(), true),
            };
            let statement = if commit { "COMMIT" } else { "ROLLBACK" };
            c.log.push(OutputEntry { time: now(), statement: statement.into(), message, is_error });
            if is_error {
                c.result_tab = ResultTab::Output;
            }
        }
        // The confirmation waiting for this end: a failed commit stops what it was for.
        if let Some(p) = self.tx_prompt.as_mut().filter(|p| p.ending.contains(&console)) {
            p.ending.retain(|&id| id != console);
            if commit && let Err(error) = result {
                self.tx_prompt = None;
                self.error(trf("Could not commit: {error}", &[("error", &error)]));
            }
        }
    }

    /// Quitting with open transactions (or commits in flight): the close is cancelled and the
    /// confirmation asked.
    pub(super) fn guard_quit(&mut self, ctx: &egui::Context) {
        if self.quit_confirmed || !ctx.input(|i| i.viewport().close_requested()) {
            return;
        }
        let all = self.consoles.iter().map(|c| c.id).collect();
        if !self.tx_guard(all, TxThen::Quit) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
    }

    /// The "Commit / Roll Back / Cancel" confirmation, then the wait for what was chosen.
    pub(super) fn tx_dialog(&mut self, ctx: &egui::Context, frame: egui::Frame) {
        let Some(prompt) = self.tx_prompt.as_mut() else { return };
        // Rollbacks that take too long: closing their sessions rolls them back too.
        if prompt.deadline.is_some_and(|d| Instant::now() >= d) {
            for id in std::mem::take(&mut prompt.ending) {
                self.worker.send(Request::ReleaseConsole { console: id });
                if let Some(c) = self.consoles.iter_mut().find(|c| c.id == id) {
                    c.tx_released |= c.running || c.ending_tx;
                    c.ending_tx = false;
                    c.tx = None;
                }
            }
        }
        let consoles = &self.consoles;
        if let Some(p) = self.tx_prompt.as_mut() {
            p.consoles.retain(|id| consoles.iter().any(|c| c.id == *id && c.may_hold_tx() && !c.ending_tx));
        }
        let Some(prompt) = self.tx_prompt.as_ref() else { return };
        // Answered and ended (or gone with their connection): go ahead.
        if prompt.consoles.is_empty() && prompt.ending.is_empty() {
            if let Some(prompt) = self.tx_prompt.take() {
                self.tx_then(ctx, prompt.then);
            }
            return;
        }
        if prompt.deadline.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        let title = |id: &u64| self.consoles.iter().find(|c| c.id == *id).map(|c| c.title.as_str());
        let names: Vec<&str> = prompt.consoles.iter().filter_map(title).collect();
        let running: Vec<u64> = prompt.consoles.iter().copied().filter(|id| self.consoles.iter().any(|c| c.id == *id && c.running)).collect();
        let first = match names.as_slice() {
            [] => None,
            [one] => Some(trf("“{console}” has a transaction that is not committed yet.", &[("console", one)])),
            _ => Some(trf("{n} consoles have transactions that are not committed yet.", &[("n", &names.len())])),
        };
        let second = match prompt.then {
            TxThen::Close(_) | TxThen::CloseTabs(..) | TxThen::CloseAll => tr("Commit the changes or roll them back before the tab closes."),
            TxThen::Auto(_) => tr("Commit the changes or roll them back before switching to Auto mode."),
            TxThen::Source(..) => tr("Commit the changes or roll them back before changing the data source."),
            TxThen::Disconnect(_) => tr("Commit the changes or roll them back before disconnecting."),
            TxThen::Connections { .. } => tr("Commit the changes or roll them back before the data source is saved or deleted."),
            TxThen::Quit => tr("Commit the changes or roll them back before DataCraft quits."),
        };
        let busy = running.first().and_then(title).map(|name| trf("A statement is still running in “{console}”: stop it before committing.", &[("console", &name)]));
        // Without a native cancel, stopping closes the session: say so, and Commit stays possible
        // once the statement finishes by itself.
        let drops = running.iter().filter_map(|id| self.consoles.iter().find(|c| c.id == *id)).find(|c| c.conn.and_then(|id| self.conn(id)).is_some_and(|s| !s.cfg.driver.has_native_cancel()));
        let drops = drops.map(|c| trf("Stopping the statement in “{console}” closes its session, which rolls the transaction back. Wait for it to finish to commit.", &[("console", &c.title)]));
        let waiting = (!prompt.ending.is_empty()).then(|| match prompt.chosen {
            Some(true) => tr("Committing…"),
            Some(false) => tr("Rolling back…"),
            None => tr("Waiting for the transaction to end…"),
        });
        let asking = first.is_some();
        // Stop, Cancel and Escape always work: nothing here waits without a way out.
        let mut decision = None;
        egui::Modal::new(egui::Id::new("confirm_transaction")).frame(frame).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.label(RichText::new(tr("Open transaction")).font(theme::bold(15.0)));
            ui.add_space(8.0);
            if let Some(first) = first {
                ui.label(first);
                ui.add_space(4.0);
                ui.label(second);
            }
            for line in [busy, drops].into_iter().flatten() {
                ui.add_space(4.0);
                ui.label(RichText::new(line).color(theme::pal(ui).warn));
            }
            if let Some(waiting) = waiting {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(14.0));
                    ui.label(RichText::new(waiting).color(theme::pal(ui).muted));
                });
            }
            ui.add_space(16.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                if asking {
                    if ui.add_enabled_ui(running.is_empty(), |ui| theme::primary_button(ui, tr("Commit"))).inner.clicked() {
                        decision = Some(TxAnswer::Commit);
                    }
                    if theme::secondary_button(ui, tr("Roll Back")).clicked() {
                        decision = Some(TxAnswer::RollBack);
                    }
                }
                if !running.is_empty() && theme::secondary_button(ui, tr("Stop Query")).clicked() {
                    decision = Some(TxAnswer::Stop);
                }
                if theme::secondary_button(ui, tr("Cancel")).clicked() {
                    decision = Some(TxAnswer::Cancel);
                }
            });
        });
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            decision = Some(TxAnswer::Cancel);
        }
        let Some(decision) = decision else { return };
        let Some(prompt) = self.tx_prompt.as_mut() else { return };
        match decision {
            // In-flight ends go on (and report in their console); only `then` is dropped.
            TxAnswer::Cancel => self.tx_prompt = None,
            TxAnswer::Stop => {
                for console in running {
                    self.worker.send(Request::Cancel { console });
                }
            }
            TxAnswer::Commit | TxAnswer::RollBack => {
                let commit = decision == TxAnswer::Commit;
                let ids = std::mem::take(&mut prompt.consoles);
                prompt.chosen = Some(commit);
                if !commit {
                    prompt.deadline = Some(Instant::now() + ROLLBACK_WAIT);
                }
                let mut ending = Vec::new();
                for id in ids {
                    let Some(i) = self.consoles.iter().position(|c| c.id == id) else { continue };
                    // A statement waiting for a lock would hold the rollback back: stop it first.
                    if !commit && self.consoles[i].running {
                        self.worker.send(Request::Cancel { console: id });
                    }
                    if self.end_transaction(i, commit) {
                        ending.push(id);
                    } else if !commit {
                        // A first run still in flight: its session goes, rolling back what it began.
                        self.worker.send(Request::ReleaseConsole { console: id });
                        let c = &mut self.consoles[i];
                        c.tx_released = true;
                        c.log.push(OutputEntry { time: now(), statement: "ROLLBACK".into(), message: tr("Transaction rolled back").into(), is_error: false });
                    }
                }
                if let Some(prompt) = self.tx_prompt.as_mut() {
                    prompt.ending.extend(ending);
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TxAnswer {
    Commit,
    RollBack,
    Stop,
    Cancel,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{ConnectionConfig, DatabaseInfo, Driver, ObjectKind, SchemaInfo, TableInfo};
    use crate::ui::results::ResultView;
    use egui_kittest::{Harness, kittest::Queryable};
    use std::time::{Duration, Instant};

    fn open_tx(statements: usize) -> Option<TxInfo> {
        Some(TxInfo { began: Instant::now() - Duration::from_secs(83), statements, aborted: false })
    }

    /// Four consoles; "Query 2" is in Manual mode on a connected PostgreSQL with `tx`.
    fn app_with_tx(tx: Option<TxInfo>) -> DataCraftApp {
        let mut app = super::super::tests::tabs_app();
        let mut conn = super::super::ConnState::new(ConnectionConfig::new(7, Driver::Postgres));
        conn.status = ConnStatus::Connected;
        app.conns.push(conn);
        let c = &mut app.consoles[1];
        c.conn = Some(7);
        c.manual_tx = true;
        c.tx = tx;
        app
    }

    fn dialog_harness(app: DataCraftApp) -> Harness<'static, DataCraftApp> {
        let mut first = true;
        let mut h = Harness::builder().with_size(egui::vec2(900.0, 600.0)).build_ui_state(
            move |ui, app: &mut DataCraftApp| {
                if std::mem::take(&mut first) {
                    theme::install_fonts(ui.ctx());
                    return;
                }
                let frame = egui::Frame::popup(ui.style());
                app.tx_dialog(ui.ctx(), frame);
            },
            app,
        );
        h.run_steps(3);
        h
    }

    #[test]
    fn closing_a_tab_asks_and_rolls_back_or_cancels() {
        let mut app = app_with_tx(open_tx(2));
        app.close_console(1);
        assert_eq!(app.consoles.len(), 4, "nothing closes before the answer");
        assert_eq!(app.tx_prompt.as_ref().map(|p| (p.consoles.clone(), p.then.clone())), Some((vec![2], TxThen::Close(2))));
        let mut h = dialog_harness(app);
        assert!(h.query_by_label("“Query 2” has a transaction that is not committed yet.").is_some());
        h.get_by_label("Cancel").click();
        h.run_steps(2);
        assert!(h.state().tx_prompt.is_none());
        assert!(h.state().consoles[1].tx.is_some(), "Cancel keeps the transaction and the tab");

        h.state_mut().close_console(1);
        h.run_steps(2);
        h.get_by_label("Roll Back").click();
        h.run_steps(2);
        // The rollback is sent and waited for before the tab closes.
        let p = h.state().tx_prompt.as_ref().expect("waiting for the rollback");
        assert_eq!((p.chosen, p.ending.clone()), (Some(false), vec![2]));
        assert!(p.deadline.is_some() && h.state().consoles[1].ending_tx);
        assert!(h.query_by_label("Rolling back…").is_some());
        h.state_mut().transaction_ended(2, false, Ok(()), None, Duration::from_millis(2));
        h.run_steps(3);
        let app = h.state();
        assert!(app.tx_prompt.is_none());
        assert_eq!(app.consoles.iter().map(|c| c.id).collect::<Vec<_>>(), [1, 3, 4]);
    }

    #[test]
    fn commit_waits_for_the_worker_before_going_ahead() {
        let mut app = app_with_tx(open_tx(1));
        assert!(!app.tx_guard(vec![2], TxThen::Auto(2)));
        let mut h = dialog_harness(app);
        h.get_by_label("Commit").click();
        h.run_steps(2);
        assert!(h.state().consoles[1].ending_tx && h.state().consoles[1].is_busy());
        assert!(h.state().tx_prompt.as_ref().is_some_and(|p| p.chosen == Some(true) && p.ending == [2] && p.deadline.is_none()));
        // A failed commit keeps the transaction, the mode and the console as they were.
        h.state_mut().transaction_ended(2, true, Err("deadlock".into()), open_tx(1), Duration::from_millis(3));
        h.run_steps(2);
        let c = &h.state().consoles[1];
        assert!(h.state().tx_prompt.is_none() && c.manual_tx && c.tx.is_some() && !c.ending_tx);
        assert!(c.log.last().is_some_and(|e| e.is_error && e.statement == "COMMIT"));

        assert!(!h.state_mut().tx_guard(vec![2], TxThen::Auto(2)));
        h.run_steps(2);
        h.get_by_label("Commit").click();
        h.run_steps(2);
        h.state_mut().transaction_ended(2, true, Ok(()), None, Duration::from_millis(3));
        h.run_steps(2);
        let c = &h.state().consoles[1];
        assert!(h.state().tx_prompt.is_none() && !c.manual_tx && c.tx.is_none(), "committed, then back to Auto");
        assert!(c.log.last().is_some_and(|e| !e.is_error && e.message.starts_with("Transaction committed")));
    }

    #[test]
    fn losing_the_connection_drops_the_transaction_and_tells_the_user() {
        let mut app = app_with_tx(open_tx(4));
        assert!(!app.tx_guard(app.consoles_on(7), TxThen::Disconnect(7)));
        app.drop_transactions(7);
        let c = &app.consoles[1];
        assert!(c.tx.is_none() && c.manual_tx && c.result_tab == ResultTab::Output);
        assert!(c.log.last().is_some_and(|e| e.is_error && e.message.contains("rolled back")));
        assert!(app.tx_prompt.as_ref().is_some_and(|p| p.consoles.is_empty() && p.ending.is_empty()), "the prompt has nothing left to ask");
        // Switching the source of a console without a transaction needs no confirmation.
        app.tx_prompt = None;
        assert!(app.tx_guard(vec![2], TxThen::Source(2, None, None)));
        app.set_console_source(2, None, None);
        assert_eq!((app.consoles[1].conn, app.consoles[1].manual_tx), (None, true));
        // A report for a console that left Manual mode (or closed) is ignored.
        app.set_manual(2, false);
        app.transaction_reported(2, open_tx(1), None);
        app.transaction_reported(99, open_tx(1), Some(TxNote::Lost));
        assert!(app.consoles[1].tx.is_none());
    }

    #[test]
    fn a_running_statement_is_stopped_before_commit_and_the_prompt_stays_escapable() {
        let mut app = app_with_tx(open_tx(2));
        app.consoles[1].running = true;
        app.close_console(1);
        let mut h = dialog_harness(app);
        assert!(h.query_by_label("A statement is still running in “Query 2”: stop it before committing.").is_some());
        // Commit is disabled while it runs: nothing is sent.
        h.get_by_label("Commit").click();
        h.run_steps(2);
        assert!(h.state().tx_prompt.as_ref().is_some_and(|p| p.chosen.is_none()) && !h.state().consoles[1].ending_tx);
        h.get_by_label("Stop Query").click();
        h.run_steps(2);
        assert!(h.state().tx_prompt.is_some(), "Stop Query cancels the statement and keeps asking");
        h.key_press(Key::Escape);
        h.run_steps(2);
        assert!(h.state().tx_prompt.is_none(), "Escape always leaves the prompt");
        // Roll Back stops the statement and waits (bounded) for the rollback behind it.
        h.state_mut().close_console(1);
        h.run_steps(2);
        h.get_by_label("Roll Back").click();
        h.run_steps(2);
        let p = h.state().tx_prompt.as_ref().expect("waiting");
        assert!(p.ending == [2] && p.deadline.is_some() && h.state().consoles[1].ending_tx);
        h.key_press(Key::Escape);
        h.run_steps(2);
        assert!(h.state().tx_prompt.is_none() && h.state().consoles.len() == 4, "Cancel while waiting keeps the tab");
    }

    #[test]
    fn a_commit_in_flight_is_waited_for_and_reported_as_committed() {
        let mut app = app_with_tx(open_tx(3));
        assert!(app.end_transaction(1, true), "Commit from the toolbar");
        app.close_console(1);
        let p = app.tx_prompt.as_ref().expect("the close waits for the commit");
        assert!(p.consoles.is_empty() && p.ending == [2]);
        assert!(!app.tx_guard(vec![2], TxThen::Quit), "quitting waits for it too");
        let mut h = dialog_harness(app);
        assert!(h.query_by_label("Roll Back").is_none() && h.query_by_label("Waiting for the transaction to end…").is_some());
        h.state_mut().transaction_ended(2, true, Ok(()), None, Duration::from_millis(2));
        h.run_steps(3);
        assert_eq!(h.state().consoles.len(), 3, "closed once committed");
        // A failed commit in flight stops the close.
        let mut app = app_with_tx(open_tx(1));
        app.end_transaction(1, true);
        app.close_console(1);
        app.transaction_ended(2, true, Err("serialization failure".into()), open_tx(1), Duration::from_millis(2));
        assert!(app.tx_prompt.is_none() && app.consoles.len() == 4 && app.consoles[1].tx.is_some());
    }

    #[test]
    fn quitting_with_a_rollback_waits_a_bounded_time() {
        let app = app_with_tx(open_tx(1));
        let mut h = dialog_harness(app);
        assert!(!h.state_mut().tx_guard(vec![1, 2, 3, 4], TxThen::Quit));
        h.run_steps(2);
        h.get_by_label("Roll Back").click();
        h.run_steps(2);
        assert!(!h.state().quit_confirmed, "the rollback reply is waited for");
        // No reply in time: the session is released (closing it rolls back) and DataCraft quits.
        if let Some(p) = h.state_mut().tx_prompt.as_mut() {
            p.deadline = Some(Instant::now() - Duration::from_millis(1));
        }
        h.run_steps(2);
        let app = h.state();
        assert!(app.quit_confirmed && app.tx_prompt.is_none());
        assert!(app.consoles[1].tx.is_none() && !app.consoles[1].ending_tx && app.consoles[1].tx_released);
    }

    #[test]
    fn replies_from_a_released_session_are_stale() {
        let mut app = app_with_tx(None);
        app.consoles[1].running = true;
        // Disconnected while the first statement still runs.
        app.drop_transactions(7);
        assert!(app.consoles[1].tx_released);
        app.transaction_reported(2, open_tx(1), None);
        assert!(app.consoles[1].tx.is_none(), "the rolled-back transaction doesn't come back");
        // The next run's reply counts again.
        app.transaction_reported(2, open_tx(1), None);
        assert!(app.consoles[1].tx.is_some());
    }

    #[test]
    fn switching_or_editing_the_data_source_asks_first() {
        let mut app = app_with_tx(open_tx(1));
        for (id, driver) in [(8, Driver::Sqlite), (9, Driver::Redis)] {
            let mut conn = super::super::ConnState::new(ConnectionConfig::new(id, driver));
            conn.status = ConnStatus::Connected;
            app.conns.push(conn);
        }
        app.active = 1;
        // The data-source picker of the window header.
        app.set_active_connection(8);
        assert_eq!(app.tx_prompt.as_ref().map(|p| p.then.clone()), Some(TxThen::Source(2, Some(8), None)));
        assert_eq!(app.consoles[1].conn, Some(7));
        app.tx_prompt = None;
        // Without a transaction it switches, and Manual mode ends on an engine without transactions.
        app.consoles[1].tx = None;
        app.set_active_connection(9);
        assert_eq!((app.consoles[1].conn, app.consoles[1].manual_tx), (Some(9), false));

        // Saving (which reconnects) or deleting a data source with an open transaction asks too.
        let mut app = app_with_tx(open_tx(1));
        let mut edited = app.conns[0].cfg.clone();
        edited.name = "renamed".into();
        app.connections_changed(vec![edited], Vec::new(), true);
        assert!(matches!(app.tx_prompt.as_ref().map(|p| &p.then), Some(TxThen::Connections { close_dialog: true, .. })));
        assert_ne!(app.conns[0].cfg.name, "renamed", "nothing saved before the answer");
        app.tx_prompt = None;
        app.connections_changed(Vec::new(), vec![7], false);
        assert!(app.tx_prompt.is_some() && app.conns.len() == 1, "nothing deleted before the answer");
        app.tx_prompt = None;
        app.consoles[1].tx = None;
        app.connections_changed(Vec::new(), vec![7], false);
        assert!(app.conns.is_empty() && app.consoles[1].conn.is_none());
    }

    #[test]
    fn a_first_run_in_flight_is_asked_about_and_logged() {
        let mut app = app_with_tx(None);
        app.consoles[1].running = true;
        app.close_console(1);
        assert!(app.tx_prompt.as_ref().is_some_and(|p| p.consoles == [2]), "it may have begun a transaction");
        let mut h = dialog_harness(app);
        h.get_by_label("Roll Back").click();
        h.run_steps(3);
        let app = h.state();
        assert!(app.tx_prompt.is_none() && app.consoles.len() == 3, "released (rolled back), then closed");
        // A run that ends without a transaction leaves nothing to ask: the prompt goes ahead.
        let mut app = app_with_tx(None);
        app.consoles[1].running = true;
        assert!(!app.tx_guard(vec![2], TxThen::Auto(2)));
        let mut h = dialog_harness(app);
        h.state_mut().consoles[1].running = false;
        h.run_steps(3);
        assert!(h.state().tx_prompt.is_none() && !h.state().consoles[1].manual_tx);
        // Disconnecting during the first run says the work was rolled back.
        let mut app = app_with_tx(None);
        app.consoles[1].running = true;
        app.drop_transactions(7);
        assert!(app.consoles[1].log.last().is_some_and(|e| e.is_error && e.message.contains("rolled back")));
    }

    #[test]
    fn grids_follow_the_transaction_mode() {
        let mut app = app_with_tx(None);
        app.consoles[1].manual_tx = false;
        let rs = crate::db::ResultSet { statement: "select id from t".into(), columns: vec!["id".into()], rows: vec![vec![Some("1".into())]], ..Default::default() };
        let mut view = ResultView::new(rs, 1);
        view.set_edit_target(Ok(crate::ui::editing::EditTarget { schema: "public".into(), table: "t".into(), col_names: vec!["id".into()], key_cols: vec![0], binary_cols: vec![false], has_pk: true, insert_only: false }));
        app.consoles[1].results.push(view);
        app.set_manual(2, true);
        let manual = tr("Grid editing is off in Manual transaction mode: change rows with SQL statements");
        assert!(app.consoles[1].results[0].edit.is_none() && app.consoles[1].results[0].readonly_reason.as_deref() == Some(manual));
        app.set_manual(2, false);
        assert_ne!(app.consoles[1].results[0].readonly_reason.as_deref(), Some(manual), "checked again in Auto mode");
    }

    #[test]
    fn statements_that_need_auto_mode_are_refused_before_they_abort_the_transaction() {
        let mut app = app_with_tx(open_tx(2));
        app.run_statements(1, vec!["VACUUM ANALYZE orders".into()]);
        let c = &app.consoles[1];
        assert!(!c.running && c.tx.is_some(), "nothing sent");
        let e = c.log.last().expect("explained");
        assert!(e.is_error && e.statement == "VACUUM ANALYZE orders" && e.message.contains("Auto mode"));
        // Auto consoles send it as before.
        app.consoles[1].manual_tx = false;
        app.run_statements(1, vec!["VACUUM ANALYZE orders".into()]);
        assert!(app.consoles[1].running);
    }

    #[test]
    fn stopping_without_a_native_cancel_is_explained() {
        let mut app = app_with_tx(open_tx(1));
        app.conns[0].cfg.driver = Driver::MsSql;
        app.consoles[1].running = true;
        app.close_console(1);
        let h = dialog_harness(app);
        assert!(h.query_by_label("Stopping the statement in “Query 2” closes its session, which rolls the transaction back. Wait for it to finish to commit.").is_some());
        assert!(h.query_by_label("Stop Query").is_some());
    }

    /// Synthetic workspace for the captures: a Manual console with an open transaction.
    fn screenshot_app(dark: bool) -> DataCraftApp {
        let mut app = super::super::tests::tabs_app();
        app.dark = dark;
        app.demo_explorer_w = Some(240.0);
        app.consoles.clear();
        let columns = |names: &[&str]| names.iter().map(|n| crate::db::ColumnInfo { name: (*n).into(), data_type: "text".into(), nullable: true, primary_key: *n == "id" }).collect::<Vec<_>>();
        let table = |name: &str, cols: &[&str]| TableInfo { name: name.into(), kind: ObjectKind::Table, columns: columns(cols) };
        let mut cfg = ConnectionConfig::new(7, Driver::Postgres);
        cfg.name = "Orders (staging)".into();
        cfg.database = "shop".into();
        let mut conn = super::super::ConnState::new(cfg);
        conn.status = ConnStatus::Connected;
        conn.databases = vec![DatabaseInfo {
            name: "shop".into(),
            current: true,
            schemas: Some(vec![SchemaInfo {
                name: "public".into(),
                tables: vec![
                    table("customers", &["id", "name", "email"]),
                    table("order_events", &["id", "order_id", "kind"]),
                    table("orders", &["id", "customer", "status", "total"]),
                ],
            }]),
        }];
        app.conns.push(conn);
        app.explorer.expanded.extend(["c:7".into(), "d:7:shop".into(), "s:7:shop:public".into(), "g:7:shop:public:t".into()]);
        let text = "-- Move paid orders to fulfillment; check them before committing\n\
                    UPDATE orders SET status = 'fulfilling'\n WHERE status = 'paid' AND total >= 100;\n\n\
                    INSERT INTO order_events (order_id, kind)\nSELECT id, 'fulfilling' FROM orders WHERE status = 'fulfilling';\n\n\
                    SELECT id, customer, status, total\n  FROM orders\n WHERE status = 'fulfilling'\n ORDER BY total DESC;";
        let mut c = crate::ui::console::Console::new(1, "Fulfillment fix".into(), text.into(), Some(7));
        c.manual_tx = true;
        c.tx = open_tx(3);
        let rows = [(48, "Customer 48", "1200.00"), (45, "Customer 45", "1125.00"), (42, "Customer 42", "1050.00"), (40, "Customer 40", "1000.00"), (37, "Customer 37", "925.00"), (35, "Customer 35", "875.00"), (31, "Customer 31", "775.00"), (29, "Customer 29", "725.00")];
        let rs = crate::db::ResultSet {
            statement: "SELECT id, customer, status, total FROM orders WHERE status = 'fulfilling' ORDER BY total DESC".into(),
            columns: vec!["id".into(), "customer".into(), "status".into(), "total".into()],
            rows: rows.iter().map(|(id, name, total)| vec![Some(id.to_string()), Some((*name).into()), Some("fulfilling".into()), Some((*total).into())]).collect(),
            elapsed: Duration::from_millis(4),
            ..Default::default()
        };
        let mut view = ResultView::new(rs, 41);
        view.set_edit_target(Err(tr("Grid editing is off in Manual transaction mode: change rows with SQL statements").into()));
        c.results.push(view);
        c.result_tab = ResultTab::Result(0);
        for (statement, message) in [
            ("UPDATE orders SET status = 'fulfilling' WHERE status = 'paid' AND total >= 100", "8 rows affected in 6 ms"),
            ("INSERT INTO order_events (order_id, kind) SELECT id, 'fulfilling' FROM orders WHERE status = 'fulfilling'", "8 rows affected in 3 ms"),
            ("SELECT id, customer, status, total FROM orders WHERE status = 'fulfilling' ORDER BY total DESC", "8 rows retrieved in 4 ms"),
        ] {
            c.log.push(OutputEntry { time: "10:42:07".into(), statement: statement.into(), message: message.into(), is_error: false });
        }
        app.consoles.push(c);
        app.consoles.push(crate::ui::console::Console::new(2, "Reports".into(), "SELECT count(*) FROM orders;".into(), Some(7)));
        app.active = 0;
        app.next_console_id = 3;
        app
    }

    fn app_harness(app: DataCraftApp, size: egui::Vec2) -> Harness<'static, DataCraftApp> {
        let dark = app.dark;
        let mut first = true;
        let mut frame = eframe::Frame::_new_kittest();
        let mut h = Harness::builder().with_size(size).build_ui_state(
            move |ui, app: &mut DataCraftApp| {
                if std::mem::take(&mut first) {
                    theme::install_fonts(ui.ctx());
                    theme::apply(ui.ctx(), dark);
                    return;
                }
                // The app owns its full root layout; remove the harness's outer padding.
                let bounds = ui.ctx().content_rect();
                let mut root = ui.new_child(egui::UiBuilder::new().max_rect(bounds));
                root.set_clip_rect(bounds);
                eframe::App::ui(app, &mut root, &mut frame);
            },
            app,
        );
        h.run_steps(4);
        h
    }

    /// The documentation captures: a Manual console with an open transaction (dark and light),
    /// the Tx dropdown, a failed PostgreSQL transaction and the confirmation when its tab closes.
    #[test]
    #[ignore = "Requires a GPU; renders the Manual transaction mode of the SQL console without a database"]
    fn render_manual_transactions() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/transaction-snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        let size = egui::vec2(1440.0, 860.0);
        let mut light = app_harness(screenshot_app(false), size);
        light.render().unwrap().save(dir.join("manual-console-light.png")).unwrap();

        let mut h = app_harness(screenshot_app(true), size);
        assert!(h.query_by_label("Commit").is_some() && h.query_by_label("Roll Back").is_some());
        h.render().unwrap().save(dir.join("manual-console-dark.png")).unwrap();

        h.get_by_label("Tx: Manual").click();
        h.run_steps(3);
        assert!(h.query_by_label("Auto").is_some());
        h.render().unwrap().save(dir.join("tx-mode-menu-dark.png")).unwrap();
        h.key_press(egui::Key::Escape);
        h.hover_at(egui::pos2(1200.0, 420.0));
        h.run_steps(3);

        let c = &mut h.state_mut().consoles[0];
        if let Some(tx) = c.tx.as_mut() {
            tx.aborted = true;
        }
        c.log.push(OutputEntry { time: "10:43:15".into(), statement: "UPDATE orders SET total = total / 0 WHERE id = 48".into(), message: "ERROR: division by zero".into(), is_error: true });
        c.result_tab = ResultTab::Output;
        h.run_steps(3);
        h.render().unwrap().save(dir.join("failed-transaction-dark.png")).unwrap();

        let c = &mut h.state_mut().consoles[0];
        c.log.pop();
        c.result_tab = ResultTab::Result(0);
        if let Some(tx) = c.tx.as_mut() {
            tx.aborted = false;
        }
        h.state_mut().close_console(0);
        h.run_steps(3);
        assert!(h.query_by_label("Open transaction").is_some());
        h.render().unwrap().save(dir.join("close-tab-confirm-dark.png")).unwrap();

        // While a statement still runs, Commit waits for Stop Query.
        h.key_press(egui::Key::Escape);
        h.run_steps(2);
        let c = &mut h.state_mut().consoles[0];
        c.running = true;
        c.run_started = Some(Instant::now() - Duration::from_secs(12));
        h.state_mut().close_console(0);
        h.run_steps(3);
        assert!(h.query_by_label("Stop Query").is_some());
        h.render().unwrap().save(dir.join("close-tab-running-dark.png")).unwrap();
    }
}
