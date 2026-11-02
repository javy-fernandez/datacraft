//! Manual transactions of SQL consoles: the app side of `Request::EndTransaction` and the
//! "Commit / Roll Back / Cancel" confirmation that guards closing, switching and quitting.

use super::{ConnStatus, DataCraftApp, now};
use crate::i18n::{tr, trf};
use crate::ui::console::{CloseResults, ResultTab};
use crate::ui::results::{self, OutputEntry};
use crate::ui::theme;
use crate::worker::{ConnId, Request, TxInfo, TxNote};
use egui::{Key, RichText};

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
    Quit,
}

/// "Commit / Roll Back / Cancel" before `then`.
pub(super) struct TxPrompt {
    /// Consoles whose transaction is open (those still committing once Commit was chosen).
    pub consoles: Vec<u64>,
    pub then: TxThen,
    pub committing: bool,
}

impl DataCraftApp {
    /// true if `then` can go ahead now; with open Manual transactions among `consoles`, asks to
    /// commit or roll them back first (and goes ahead afterwards).
    pub(super) fn tx_guard(&mut self, consoles: Vec<u64>, then: TxThen) -> bool {
        let open: Vec<u64> = consoles.into_iter().filter(|id| self.consoles.iter().any(|c| c.id == *id && c.tx.is_some())).collect();
        if open.is_empty() {
            return true;
        }
        if self.tx_prompt.is_none() {
            self.tx_prompt = Some(TxPrompt { consoles: open, then, committing: false });
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
            TxThen::Quit => {
                self.quit_confirmed = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
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
    }

    /// A Manual console moves to another connection / database: its own session is released
    /// and the next statement opens one there.
    pub(super) fn set_console_source(&mut self, id: u64, conn: Option<ConnId>, db: Option<String>) {
        let target = conn.and_then(|c| self.conn(c)).map(|c| (c.cfg.driver.has_transactions(), c.status.clone()));
        let Some(c) = self.consoles.iter_mut().find(|c| c.id == id) else { return };
        self.worker.send(Request::ReleaseConsole { console: id });
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

    /// Commit (true) or Roll Back from a console's toolbar; false if it has nothing to end.
    pub(super) fn end_transaction(&mut self, idx: usize, commit: bool) -> bool {
        let Some(c) = self.consoles.get_mut(idx).filter(|c| c.tx.is_some() && !c.ending_tx) else { return false };
        c.ending_tx = true;
        c.run_started = Some(std::time::Instant::now());
        self.worker.send(Request::EndTransaction { console: c.id, commit });
        true
    }

    /// The connection closed or was lost: its consoles' transactions are gone (the server rolls
    /// them back) and their own sessions are released.
    pub(super) fn drop_transactions(&mut self, conn: ConnId) {
        for c in self.consoles.iter_mut().filter(|c| c.conn == Some(conn) && c.manual_tx) {
            self.worker.send(Request::ReleaseConsole { console: c.id });
            c.ending_tx = false;
            if c.tx.take().is_some() {
                c.log.push(OutputEntry { time: now(), statement: String::new(), message: tr("The connection closed, so the open transaction was rolled back").into(), is_error: true });
                c.result_tab = ResultTab::Output;
            }
        }
        let consoles = &self.consoles;
        if let Some(p) = self.tx_prompt.as_mut() {
            p.consoles.retain(|id| consoles.iter().any(|c| c.id == *id && c.tx.is_some()));
        }
    }

    /// The transaction after a Manual run, and why it ended if it ended by itself.
    pub(super) fn transaction_reported(&mut self, console: u64, tx: Option<TxInfo>, note: Option<TxNote>) {
        // A console closed (or back in Auto mode) while it ran: its session goes too.
        let Some(c) = self.consoles.iter_mut().find(|c| c.id == console && c.manual_tx) else {
            self.worker.send(Request::ReleaseConsole { console });
            return;
        };
        c.tx = tx;
        let message = match note {
            Some(TxNote::Lost) => Some((tr("The session closed, so the open transaction was rolled back"), true)),
            Some(TxNote::ImplicitCommit) => Some((tr("The statement committed the open transaction implicitly"), false)),
            Some(TxNote::RolledBack) => Some((tr("The server rolled back the transaction after the error"), true)),
            None => None,
        };
        if let Some((message, is_error)) = message {
            c.log.push(OutputEntry { time: now(), statement: String::new(), message: message.into(), is_error });
        }
    }

    pub(super) fn transaction_ended(&mut self, console: u64, commit: bool, result: Result<(), String>, tx: Option<TxInfo>, elapsed: std::time::Duration) {
        if let Some(c) = self.consoles.iter_mut().find(|c| c.id == console) {
            c.ending_tx = false;
            c.run_started = None;
            c.tx = tx;
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
        // The confirmation waiting for this commit.
        if let Some(p) = self.tx_prompt.as_mut().filter(|p| p.committing && p.consoles.contains(&console)) {
            match result {
                Ok(()) => p.consoles.retain(|&id| id != console),
                Err(error) => {
                    self.tx_prompt = None;
                    self.error(trf("Could not commit: {error}", &[("error", &error)]));
                }
            }
        }
    }

    /// Quitting with open transactions: the close is cancelled and the confirmation asked.
    pub(super) fn guard_quit(&mut self, ctx: &egui::Context) {
        if self.quit_confirmed || !ctx.input(|i| i.viewport().close_requested()) {
            return;
        }
        let all = self.consoles.iter().map(|c| c.id).collect();
        if !self.tx_guard(all, TxThen::Quit) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
    }

    /// The "Commit / Roll Back / Cancel" confirmation.
    pub(super) fn tx_dialog(&mut self, ctx: &egui::Context, frame: egui::Frame) {
        let Some(prompt) = self.tx_prompt.as_ref() else { return };
        // Committed (or gone with their connection): go ahead.
        if prompt.consoles.is_empty() {
            if let Some(prompt) = self.tx_prompt.take() {
                self.tx_then(ctx, prompt.then);
            }
            return;
        }
        let names: Vec<&str> = prompt.consoles.iter().filter_map(|id| self.consoles.iter().find(|c| c.id == *id)).map(|c| c.title.as_str()).collect();
        let first = match names.as_slice() {
            [one] => trf("“{console}” has a transaction that is not committed yet.", &[("console", one)]),
            _ => trf("{n} consoles have transactions that are not committed yet.", &[("n", &names.len())]),
        };
        let second = match prompt.then {
            TxThen::Close(_) | TxThen::CloseTabs(..) | TxThen::CloseAll => tr("Commit the changes or roll them back before the tab closes."),
            TxThen::Auto(_) => tr("Commit the changes or roll them back before switching to Auto mode."),
            TxThen::Source(..) => tr("Commit the changes or roll them back before changing the data source."),
            TxThen::Disconnect(_) => tr("Commit the changes or roll them back before disconnecting."),
            TxThen::Quit => tr("Commit the changes or roll them back before DataCraft quits."),
        };
        let committing = prompt.committing;
        let mut decision = None;
        egui::Modal::new(egui::Id::new("confirm_transaction")).frame(frame).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.label(RichText::new(tr("Open transaction")).font(theme::bold(15.0)));
            ui.add_space(8.0);
            ui.label(first);
            ui.add_space(4.0);
            ui.label(second);
            ui.add_space(16.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.add_enabled_ui(!committing, |ui| {
                    if theme::primary_button(ui, tr("Commit")).clicked() {
                        decision = Some(Some(true));
                    }
                    if theme::secondary_button(ui, tr("Roll Back")).clicked() {
                        decision = Some(Some(false));
                    }
                    if theme::secondary_button(ui, tr("Cancel")).clicked() {
                        decision = Some(None);
                    }
                });
                if committing {
                    ui.label(RichText::new(tr("Committing…")).color(theme::pal(ui).muted));
                    ui.add(egui::Spinner::new().size(14.0));
                }
            });
        });
        if !committing && ctx.input(|i| i.key_pressed(Key::Escape)) {
            decision = Some(None);
        }
        let Some(decision) = decision else { return };
        let Some(mut prompt) = self.tx_prompt.take() else { return };
        match decision {
            None => {}
            Some(true) => {
                let ids = std::mem::take(&mut prompt.consoles);
                for id in ids {
                    if let Some(i) = self.consoles.iter().position(|c| c.id == id)
                        && self.end_transaction(i, true)
                    {
                        prompt.consoles.push(id);
                    }
                }
                prompt.committing = true;
                self.tx_prompt = Some(prompt);
            }
            Some(false) => {
                // Closing the console's session rolls its transaction back.
                for id in &prompt.consoles {
                    self.worker.send(Request::ReleaseConsole { console: *id });
                    if let Some(c) = self.consoles.iter_mut().find(|c| c.id == *id) {
                        c.tx = None;
                        c.log.push(OutputEntry { time: now(), statement: "ROLLBACK".into(), message: tr("Transaction rolled back").into(), is_error: false });
                    }
                }
                self.tx_then(ctx, prompt.then);
            }
        }
    }
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
        assert!(h.state().tx_prompt.as_ref().is_some_and(|p| p.committing));
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
        assert!(app.tx_prompt.as_ref().is_some_and(|p| p.consoles.is_empty()), "the prompt has nothing left to ask");
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
    }
}
