//! "Session Monitor…" windows: opening them, their worker requests and replies.

use super::*;
use crate::ui::session_monitor::{MonitorAction, SessionMonitor};

impl DataCraftApp {
    /// Opens (or brings back) the connection's Session Monitor.
    pub(super) fn open_session_monitor(&mut self, conn: ConnId) {
        let Some(c) = self.conn(conn).filter(|c| c.status == ConnStatus::Connected) else { return };
        if !crate::db::sessions::supported(c.cfg.driver) {
            self.error(tr("Session monitoring is not available for this engine").to_string());
            return;
        }
        if self.session_monitors.iter().any(|m| m.conn == conn) {
            return;
        }
        let monitor = SessionMonitor::new(conn, c.cfg.driver, c.cfg.read_only, c.cfg.name.clone());
        self.session_monitors.push(monitor);
    }

    pub(super) fn show_session_monitors(&mut self, ctx: &egui::Context) {
        let mut closed = Vec::new();
        for i in 0..self.session_monitors.len() {
            let conn = self.session_monitors[i].conn;
            // The data source may have been edited since the window opened.
            if let Some(c) = self.conn(conn) {
                let (driver, read_only, name) = (c.cfg.driver, c.cfg.read_only, c.cfg.name.clone());
                if !crate::db::sessions::supported(driver) {
                    closed.push(conn);
                    continue;
                }
                self.session_monitors[i].sync(driver, read_only, &name);
            }
            let connected = self.conn(conn).is_some_and(|c| c.status == ConnStatus::Connected);
            for action in self.session_monitors[i].show(ctx, connected) {
                self.next_monitor_request += 1;
                let request_id = self.next_monitor_request;
                let monitor = &mut self.session_monitors[i];
                match action {
                    MonitorAction::Refresh => {
                        monitor.refresh_sent(request_id);
                        self.worker.send(Request::Sessions { conn, request_id });
                    }
                    MonitorAction::Act(action, epoch) => {
                        monitor.action_sent(request_id, action.clone());
                        self.worker.send(Request::SessionAction { conn, request_id, epoch, action });
                    }
                    MonitorAction::Close => closed.push(conn),
                }
            }
        }
        // The last window of a connection closes its monitor session; a deleted connection takes
        // its monitor with it (and the disconnect its session).
        for conn in &closed {
            self.worker.send(Request::CloseSessionMonitor(*conn));
        }
        let conns: Vec<ConnId> = self.conns.iter().map(|c| c.cfg.id).collect();
        self.session_monitors.retain(|m| !closed.contains(&m.conn) && conns.contains(&m.conn));
    }

    /// The connection was closed, lost or opened again: its monitor forgets the old list.
    pub(super) fn reset_session_monitor(&mut self, conn: ConnId) {
        if let Some(m) = self.session_monitors.iter_mut().find(|m| m.conn == conn) {
            m.reset();
        }
    }

    pub(super) fn session_list_received(&mut self, conn: ConnId, request_id: u64, epoch: u64, result: Result<crate::db::sessions::SessionList, String>) {
        if let Some(m) = self.session_monitors.iter_mut().find(|m| m.conn == conn) {
            m.receive_list(request_id, epoch, result);
        }
    }

    pub(super) fn session_action_done(&mut self, conn: ConnId, request_id: u64, result: Result<(), String>) {
        if let Some(m) = self.session_monitors.iter_mut().find(|m| m.conn == conn) {
            m.receive_action(request_id, result);
        }
    }
}

// Full-window captures, like `readme_screenshots` (Windows GPU renderer).
#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;
    use crate::db::sessions::{SessionList, SessionRow};
    use egui_kittest::{Harness, kittest::Queryable};

    /// A busy PostgreSQL server: 4310 holds a row lock in an open transaction, 4242 waits for it
    /// and 4377 waits for 4242.
    fn sessions() -> SessionList {
        let row = |id: &str, user: &str, db: &str, client: &str, app: &str, state: &str, ms: Option<u64>, wait: &str, blocked: &[&str], query: &str| SessionRow {
            id: id.into(),
            started: Some("1793000000000000".into()),
            user: user.into(),
            database: db.into(),
            client: client.into(),
            application: app.into(),
            state: state.into(),
            duration_ms: ms,
            wait: wait.into(),
            blocked_by: blocked.iter().map(|b| b.to_string()).collect(),
            query: query.into(),
            idle: state == "idle",
            datacraft: app == "DataCraft",
            ..Default::default()
        };
        let mut rows = vec![
            row("3870", "", "shop", "", "", "autovacuum worker", Some(123_400), "", &[], "autovacuum: VACUUM ANALYZE public.order_items"),
            row("3998", "app_ro", "shop", "10.0.4.30:39911", "catalog-api", "idle", None, "", &[], "SELECT id, sku, name, price FROM products WHERE sku = $1"),
            row("4012", "app_ro", "shop", "10.0.4.30:39915", "catalog-api", "idle", None, "", &[], "SELECT count(*) FROM product_images WHERE product_id = $1"),
            row(
                "4120",
                "reporting",
                "analytics",
                "10.0.6.3:61022",
                "metabase",
                "active",
                Some(401_300),
                "IO: DataFileRead",
                &[],
                "SELECT date_trunc('day', o.created_at) AS day, count(*) AS orders, sum(i.quantity * i.price) AS revenue\nFROM orders o\nJOIN order_items i ON i.order_id = o.id\nWHERE o.created_at >= now() - interval '90 days'\nGROUP BY 1\nORDER BY 1",
            ),
            row(
                "4242",
                "app_rw",
                "shop",
                "10.0.4.17:52114",
                "order-service",
                "active",
                Some(48_200),
                "Lock: transactionid",
                &["4310"],
                "UPDATE orders\nSET status = 'shipped', shipped_at = now()\nWHERE id = 918273\nRETURNING id, status, shipped_at",
            ),
            row("4310", "app_rw", "shop", "10.0.4.21:40188", "billing-worker", "idle in transaction", Some(72_900), "", &[], "UPDATE orders SET total = total - 5.00, discount_code = 'AUTUMN5' WHERE id = 918273"),
            row("4377", "app_rw", "shop", "10.0.4.17:52130", "order-service", "active", Some(31_500), "Lock: tuple", &["4242"], "UPDATE orders SET status = 'cancelled' WHERE id = 918273 AND status = 'pending'"),
            row("4455", "etl", "analytics", "10.0.7.12:44210", "airflow", "active", Some(12_400), "", &[], "INSERT INTO staging.events (event_id, kind, payload, created_at) SELECT * FROM import.events_2026_11"),
            row("4488", "dba", "shop", "192.168.1.40:50712", "DataCraft", "idle", None, "", &[], "SELECT * FROM orders WHERE id = 918273"),
            row(
                "4501",
                "dba",
                "shop",
                "192.168.1.40:50731",
                "DataCraft",
                "active",
                Some(3),
                "",
                &[],
                "SELECT /* DataCraft session monitor */ pid::text AS s_id, usename::text AS s_user, datname::text AS s_db, … FROM pg_stat_activity",
            ),
        ];
        rows[9].own = true;
        SessionList { rows, notice: None }
    }

    fn fixture(dark: bool) -> DataCraftApp {
        let mut app = super::super::tests::tabs_app();
        app.dark = dark;
        app.demo_explorer_w = Some(255.0);
        app.consoles.truncate(1);
        app.consoles[0].title = "orders.sql".into();
        app.active = 0;
        let mut cfg = ConnectionConfig::new(7, Driver::Postgres);
        cfg.name = "Production".into();
        cfg.host = "db.internal".into();
        cfg.database = "shop".into();
        let mut conn = ConnState::new(cfg);
        conn.status = ConnStatus::Connected;
        let tables = ["customers", "order_items", "orders", "payments", "products", "shipments"]
            .into_iter()
            .map(|name| crate::db::TableInfo { name: name.into(), kind: crate::db::ObjectKind::Table, columns: Vec::new() })
            .collect();
        conn.databases = vec![
            DatabaseInfo { name: "shop".into(), current: true, schemas: Some(vec![SchemaInfo { name: "public".into(), tables }]) },
            DatabaseInfo { name: "analytics".into(), current: false, schemas: None },
        ];
        app.conns.push(conn);
        app.explorer.expanded.extend(["c:7".into(), "d:7:shop".into()]);
        let mut monitor = SessionMonitor::new(7, Driver::Postgres, false, "Production".into());
        monitor.show_list(sessions(), "10:42:17", "4242");
        app.session_monitors.push(monitor);
        app
    }

    fn harness(dark: bool) -> Harness<'static, DataCraftApp> {
        let mut first = true;
        let mut frame = eframe::Frame::_new_kittest();
        let mut harness = Harness::builder().with_size(vec2(1600.0, 1000.0)).build_ui_state(
            move |ui, app| {
                if std::mem::take(&mut first) {
                    theme::install_fonts(ui.ctx());
                    theme::apply(ui.ctx(), dark);
                    return;
                }
                let bounds = ui.ctx().content_rect();
                let mut root = ui.new_child(egui::UiBuilder::new().max_rect(bounds));
                root.set_clip_rect(bounds);
                eframe::App::ui(app, &mut root, &mut frame);
            },
            fixture(dark),
        );
        harness.run_steps(4);
        harness
    }

    #[test]
    #[ignore = "Requires a GPU; renders the Session Monitor with synthetic PostgreSQL sessions"]
    fn render_session_monitor() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/session-monitor-snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        let mut h = harness(true);
        assert!(h.query_by_label("Session 4310").is_some());
        h.render().unwrap().save(dir.join("session-monitor-dark.png")).unwrap();

        h.get_by_label("Session 4310").click();
        h.run_steps(3);
        h.get_by_label("Terminate Session…").click();
        h.run_steps(4);
        assert!(h.state().session_monitors[0].confirming());
        h.render().unwrap().save(dir.join("session-monitor-terminate-dark.png")).unwrap();

        let mut light = harness(false);
        light.render().unwrap().save(dir.join("session-monitor-light.png")).unwrap();

        light.state_mut().session_monitors.clear();
        light.run_steps(2);
        light.get_by_label("Production").click_secondary();
        light.run_steps(4);
        assert!(light.query_by_label("Session Monitor…").is_some());
        light.render().unwrap().save(dir.join("session-monitor-menu-light.png")).unwrap();
    }
}
