//! Confirmation before the console runs statements that act on whole tables (`UPDATE` /
//! `DELETE` without `WHERE`, `TRUNCATE`, `DROP`) and, on a Production data source, before
//! anything changes data: the app side of `sql::dangerous_statements`.

use super::{DataCraftApp, is_query, is_read_statement};
use crate::db::{ConnectionConfig, Driver, Environment};
use crate::i18n::{tr, trf};
use crate::sql::{self, Danger, Dangerous};
use crate::ui::theme;
use crate::worker::ConnId;
use egui::{Color32, Key, RichText, Stroke, vec2};

/// What runs once the user confirms.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Guarded {
    /// Statements of a console.
    Run(Vec<String>),
    /// The pending grid changes (the preview's statements).
    Changes,
}

/// An execution waiting for the user's confirmation.
pub(super) struct Confirm {
    pub console: u64,
    pub what: Guarded,
    pub dangerous: Vec<Dangerous>,
    /// The data source's name and badge colour, when it is a Production one.
    pub production: Option<(String, Color32)>,
    /// Explain Analyze, which runs the statement to measure it.
    pub analyze: bool,
}

#[derive(Default)]
pub(super) struct SafetyState {
    pub confirm: Option<Confirm>,
}

/// Name and badge colour of a Production data source.
fn production(cfg: &ConnectionConfig) -> Option<(String, Color32)> {
    theme::environment(cfg).filter(|(env, _)| *env == Environment::Production).map(|(_, color)| (cfg.name.clone(), color))
}

/// Shown at most; the rest are counted.
const LISTED: usize = 8;

/// The statement as the prompt names it (`DELETE`, `DROP TABLE`, `FLUSHALL`).
fn keyword(d: &Dangerous) -> String {
    match d.danger {
        Danger::Update => "UPDATE".into(),
        Danger::Delete => "DELETE".into(),
        Danger::Truncate => "TRUNCATE".into(),
        Danger::Drop => format!("DROP {}", d.object),
        Danger::DropColumn => "DROP COLUMN".into(),
        Danger::DropPartition => "DROP PARTITION".into(),
        Danger::MergeDelete => "MERGE … DELETE".into(),
        Danger::Replace => format!("CREATE OR REPLACE {}", d.object),
        Danger::Flush => d.object.clone(),
    }
}

/// What the statement does, after its target.
fn effect(danger: Danger) -> &'static str {
    match danger {
        Danger::Update => tr("changes every row"),
        Danger::Delete => tr("deletes every row"),
        Danger::Truncate => tr("removes every row"),
        Danger::Drop => tr("removes the object and its data"),
        Danger::DropColumn => tr("removes the column and its values"),
        Danger::DropPartition => tr("removes the partition and its rows"),
        Danger::MergeDelete => tr("deletes every matched row"),
        Danger::Replace => tr("replaces it, dropping the current data"),
        Danger::Flush => tr("removes every key"),
    }
}

impl DataCraftApp {
    /// The confirmation `statements` of `console` need before they run on `conn`, if any.
    pub(super) fn run_confirmation(&self, conn: ConnId, console: u64, statements: &[String]) -> Option<Confirm> {
        let cfg = &self.conn(conn)?.cfg;
        let dangerous: Vec<Dangerous> = if cfg.confirm_dangerous {
            statements.iter().flat_map(|s| sql::dangerous_statements(s, cfg.driver)).collect()
        } else {
            Vec::new()
        };
        let writes = || statements.iter().flat_map(|s| sql::split_statements(s, cfg.driver)).any(|s| !is_read_statement(cfg.driver, &s.text));
        let production = production(cfg).filter(|_| writes());
        (!dangerous.is_empty() || production.is_some()).then(|| Confirm { console, what: Guarded::Run(statements.to_vec()), dangerous, production, analyze: false })
    }

    /// The confirmation saving grid changes on `conn` needs (Production data sources).
    pub(super) fn changes_confirmation(&self, conn: ConnId, console: u64) -> Option<Confirm> {
        let production = production(&self.conn(conn)?.cfg)?;
        Some(Confirm { console, what: Guarded::Changes, dangerous: Vec::new(), production: Some(production), analyze: false })
    }

    /// Explain Analyze of console `idx`: it runs `statement`, so one that writes asks once,
    /// with the dangerous statement / Production prompt when it needs one, else its own.
    pub(super) fn explain_analyze(&mut self, idx: usize, statement: &str, explain: String) {
        let Some(c) = self.consoles.get(idx) else { return };
        let driver = c.conn.and_then(|id| self.conn(id)).map_or(Driver::Postgres, |c| c.cfg.driver);
        let confirm = c.conn.and_then(|conn| self.run_confirmation(conn, c.id, std::slice::from_ref(&explain)));
        if is_query(driver, statement) {
            self.run_statements(idx, vec![explain]);
        } else if let Some(confirm) = confirm {
            self.safety.confirm = Some(Confirm { analyze: true, ..confirm });
        } else {
            self.confirm_analyze = Some((idx, explain));
        }
    }

    pub(super) fn confirm_dialog(&mut self, ctx: &egui::Context, frame: egui::Frame) {
        let Some(c) = &self.safety.confirm else { return };
        let mut decision = None;
        egui::Modal::new(egui::Id::new("confirm_guarded")).frame(frame).show(ctx, |ui| {
            let p = theme::pal(ui);
            ui.set_width(480.0);
            ui.horizontal(|ui| {
                let title = if c.dangerous.is_empty() { tr("Change Production data?") } else { tr("Run dangerous statements?") };
                ui.label(RichText::new(title).font(theme::bold(15.0)));
                if let Some((_, color)) = c.production {
                    ui.add_space(6.0);
                    theme::environment_badge(ui, Environment::Production, color);
                }
            });
            ui.add_space(8.0);
            if let Some((name, _)) = &c.production {
                ui.label(match c.what {
                    Guarded::Run(_) => trf("{name} is a Production data source, and these statements change data.", &[("name", name)]),
                    Guarded::Changes => trf("{name} is a Production data source, and saving writes the pending grid changes to it.", &[("name", name)]),
                });
                ui.add_space(6.0);
            }
            if c.analyze {
                ui.label(tr("Explain Analyze runs the statement to measure it."));
                ui.add_space(6.0);
            }
            if !c.dangerous.is_empty() {
                ui.label(tr("These statements act on whole tables:"));
                ui.add_space(4.0);
                egui::Frame::NONE.fill(p.editor_bg).stroke(Stroke::new(1.0, p.control_border)).corner_radius(6).inner_margin(egui::Margin::same(8)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    for d in c.dangerous.iter().take(LISTED) {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(keyword(d)).monospace().strong().color(p.error));
                            ui.label(RichText::new(&d.target).monospace().color(p.text));
                            ui.label(RichText::new(effect(d.danger)).color(p.muted));
                        });
                    }
                    if c.dangerous.len() > LISTED {
                        ui.label(RichText::new(trf("…and {n} more", &[("n", &(c.dangerous.len() - LISTED))])).color(p.muted));
                    }
                });
                ui.add_space(6.0);
                ui.label(RichText::new(tr("Turn this check off in the data source's Options tab.")).small().color(p.muted));
            }
            ui.add_space(16.0);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 28.0), egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let label = if c.what == Guarded::Changes { tr("Submit") } else { tr("Run") };
                let run = egui::Button::new(RichText::new(label).color(Color32::WHITE)).fill(p.error).stroke(Stroke::new(1.0, p.error)).corner_radius(6).min_size(vec2(72.0, 28.0));
                if ui.add(run).clicked() {
                    decision = Some(true);
                }
                if theme::secondary_button(ui, tr("Cancel")).clicked() {
                    decision = Some(false);
                }
            });
        });
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            decision = Some(false);
        }
        let Some(ok) = decision else { return };
        let Some(c) = self.safety.confirm.take() else { return };
        match c.what {
            Guarded::Run(statements) if ok => {
                if let Some(idx) = self.consoles.iter().position(|x| x.id == c.console) {
                    self.send_statements(idx, statements, true);
                }
            }
            Guarded::Run(_) => {}
            Guarded::Changes if ok => {
                self.submit_preview(true);
                self.report_hidden_submit();
            }
            // A direct save (no preview window) is dropped; the grid keeps its changes.
            Guarded::Changes => {
                if self.preview.as_ref().is_some_and(|p| !p.visible) {
                    self.preview = None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::tabs_app;
    use super::*;
    use crate::ui::console::Console;

    fn app(driver: Driver, environment: Environment, confirm_dangerous: bool) -> DataCraftApp {
        let mut app = tabs_app();
        let mut cfg = ConnectionConfig::new(7, driver);
        cfg.name = "orders-prod".into();
        cfg.environment = environment;
        cfg.confirm_dangerous = confirm_dangerous;
        app.conns.push(super::super::ConnState::new(cfg));
        app.conns[0].status = super::super::ConnStatus::Connected;
        app.consoles = vec![Console::new(1, "Console".into(), String::new(), Some(7))];
        app.active = 0;
        app
    }

    fn sent(app: &DataCraftApp) -> bool {
        app.consoles[0].running
    }

    #[test]
    fn dangerous_statements_wait_for_confirmation() {
        let mut a = app(Driver::Postgres, Environment::None, true);
        a.run_statements(0, vec!["DELETE FROM orders".into()]);
        let c = a.safety.confirm.as_ref().expect("asks");
        assert_eq!(c.dangerous.len(), 1);
        assert!(c.production.is_none() && !sent(&a));
        // Confirming runs exactly what was held back.
        let Some(Confirm { what: Guarded::Run(statements), .. }) = a.safety.confirm.take() else { panic!() };
        a.send_statements(0, statements, true);
        assert!(sent(&a) && a.consoles[0].last_statements == ["DELETE FROM orders"]);
    }

    #[test]
    fn safe_statements_and_disabled_guard_run_at_once() {
        let mut a = app(Driver::MySql, Environment::Staging, true);
        a.run_statements(0, vec!["DELETE FROM orders WHERE id = 1".into()]);
        assert!(a.safety.confirm.is_none() && sent(&a));
        let mut a = app(Driver::MySql, Environment::Development, false);
        a.run_statements(0, vec!["TRUNCATE orders".into()]);
        assert!(a.safety.confirm.is_none() && sent(&a));
    }

    #[test]
    fn production_asks_once_for_any_write_and_never_for_reads() {
        let mut a = app(Driver::Postgres, Environment::Production, true);
        a.run_statements(0, vec!["SELECT * FROM orders".into()]);
        assert!(a.safety.confirm.is_none() && sent(&a));
        let mut a = app(Driver::Postgres, Environment::Production, true);
        a.run_statements(0, vec!["UPDATE orders SET a = 1 WHERE id = 1".into(), "INSERT INTO t VALUES (1)".into()]);
        let c = a.safety.confirm.as_ref().expect("asks");
        assert!(c.dangerous.is_empty() && c.production.as_ref().is_some_and(|(n, _)| n == "orders-prod"));
        // One prompt covers a dangerous statement on Production too, even with the check off.
        let mut a = app(Driver::Postgres, Environment::Production, false);
        a.run_statements(0, vec!["DROP TABLE orders".into()]);
        let c = a.safety.confirm.as_ref().expect("asks");
        assert!(c.dangerous.is_empty() && c.production.is_some());
        // Non-SQL engines: their own read commands.
        let mut a = app(Driver::Redis, Environment::Production, true);
        a.run_statements(0, vec!["GET k".into()]);
        assert!(a.safety.confirm.is_none());
        a.consoles[0].running = false;
        a.run_statements(0, vec!["FLUSHALL".into()]);
        assert!(a.safety.confirm.is_some());
    }

    /// Words in strings, comments and a data editor filter are not statements (review of #60).
    #[test]
    fn production_reads_mentioning_writes_run_at_once() {
        for sql in [
            "SELECT * FROM audit WHERE action = 'DELETE'",
            "SELECT 1 /* update later */",
            "SELECT note FROM orders -- insert into?",
            "EXPLAIN ANALYZE SELECT * FROM orders",
            "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) SELECT 1",
        ] {
            let mut a = app(Driver::Postgres, Environment::Production, true);
            a.run_statements(0, vec![sql.into()]);
            assert!(a.safety.confirm.is_none() && sent(&a), "{sql}");
        }
        // The data editor's reloads and pages with a filter that names a write.
        let mut a = app(Driver::Postgres, Environment::Production, true);
        let mut table = Console::new_table(2, 7, None, "public".into(), "audit".into());
        if let Some(t) = table.table.as_mut() {
            t.where_clause = "action IN ('DELETE', 'UPDATE') AND note <> 'drop table'".into();
        }
        a.consoles.push(table);
        a.reload_table(1);
        assert!(a.safety.confirm.is_none() && a.consoles[1].running);
    }

    #[test]
    fn explain_analyze_asks_once() {
        let explain = |sql: &str| format!("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {sql}");
        // A dangerous statement: only the new prompt, which says Explain Analyze runs it.
        let mut a = app(Driver::Postgres, Environment::None, true);
        a.explain_analyze(0, "DELETE FROM orders", explain("DELETE FROM orders"));
        assert!(a.safety.confirm.as_ref().is_some_and(|c| c.analyze && c.dangerous.len() == 1) && a.confirm_analyze.is_none());
        let Some(Confirm { what: Guarded::Run(statements), .. }) = a.safety.confirm.take() else { panic!() };
        a.send_statements(0, statements, true);
        assert!(sent(&a));
        // On Production, any write.
        let mut a = app(Driver::Postgres, Environment::Production, true);
        a.explain_analyze(0, "UPDATE orders SET a = 1 WHERE id = 1", explain("UPDATE orders SET a = 1 WHERE id = 1"));
        assert!(a.safety.confirm.as_ref().is_some_and(|c| c.analyze && c.production.is_some()) && a.confirm_analyze.is_none());
        // Otherwise its own prompt, and Run there doesn't ask again.
        let mut a = app(Driver::Postgres, Environment::Staging, true);
        a.explain_analyze(0, "UPDATE orders SET a = 1 WHERE id = 1", explain("UPDATE orders SET a = 1 WHERE id = 1"));
        assert!(a.safety.confirm.is_none() && a.confirm_analyze.is_some());
        let (idx, statement) = a.confirm_analyze.take().unwrap();
        a.run_statements(idx, vec![statement]);
        assert!(a.safety.confirm.is_none() && sent(&a));
        // A read runs at once, on Production too.
        let mut a = app(Driver::MySql, Environment::Production, true);
        a.explain_analyze(0, "SELECT * FROM orders", "EXPLAIN ANALYZE SELECT * FROM orders".into());
        assert!(a.safety.confirm.is_none() && a.confirm_analyze.is_none() && sent(&a));
    }

    /// A Manual transaction console asks like an Auto one before its statements join the
    /// transaction; Commit and Roll Back only end what was already confirmed.
    #[test]
    fn manual_transaction_consoles_ask_too() {
        let mut a = app(Driver::Postgres, Environment::Production, true);
        a.consoles[0].manual_tx = true;
        a.run_statements(0, vec!["UPDATE orders SET a = 1 WHERE id = 1".into()]);
        assert!(a.safety.confirm.as_ref().is_some_and(|c| c.production.is_some()) && !sent(&a));
        // Esc / Cancel: nothing ran, the transaction (none yet) is untouched.
        a.safety.confirm = None;
        a.run_statements(0, vec!["DELETE FROM orders".into()]);
        let Some(Confirm { what: Guarded::Run(statements), dangerous, .. }) = a.safety.confirm.take() else { panic!() };
        assert_eq!(dangerous.len(), 1);
        a.send_statements(0, statements, true);
        assert!(sent(&a) && a.consoles[0].manual_tx);
        // The worker opened the transaction; Commit and Roll Back don't ask.
        a.consoles[0].running = false;
        a.consoles[0].tx = Some(crate::worker::TxInfo { began: std::time::Instant::now(), statements: 1, aborted: false });
        assert!(a.end_transaction(0, true));
        assert!(a.safety.confirm.is_none());
        a.consoles[0].ending_tx = false;
        assert!(a.end_transaction(0, false));
        assert!(a.safety.confirm.is_none());
        // A statement Manual mode refuses is refused before any prompt.
        let mut a = app(Driver::Postgres, Environment::Production, true);
        a.consoles[0].manual_tx = true;
        a.run_statements(0, vec!["VACUUM orders".into()]);
        assert!(a.safety.confirm.is_none() && !sent(&a) && a.consoles[0].log.last().is_some_and(|e| e.is_error));
    }

    #[test]
    fn grid_saves_on_production_ask_first() {
        let mut a = app(Driver::Postgres, Environment::Production, true);
        a.preview = Some(super::super::PreviewState { console: 1, view: 0, statements: vec!["UPDATE t SET a = 1 WHERE id = 1".into()], error: None, applying: false, visible: false });
        a.submit_preview(false);
        assert!(matches!(a.safety.confirm.as_ref().map(|c| &c.what), Some(Guarded::Changes)));
        assert!(!a.preview.as_ref().unwrap().applying);
        a.submit_preview(true);
        assert!(a.preview.as_ref().unwrap().applying);
        let mut a = app(Driver::Postgres, Environment::Staging, true);
        a.preview = Some(super::super::PreviewState { console: 1, view: 0, statements: vec!["DELETE FROM t WHERE id = 1".into()], error: None, applying: false, visible: true });
        a.submit_preview(false);
        assert!(a.safety.confirm.is_none() && a.preview.as_ref().unwrap().applying);
    }

    /// Synthetic workspace for the captures: Production, Staging and Development data sources.
    fn screenshot_app(dark: bool) -> DataCraftApp {
        use crate::db::{ColumnInfo, DatabaseInfo, ObjectKind, SchemaInfo, TableInfo};
        let mut app = tabs_app();
        app.dark = dark;
        app.demo_explorer_w = Some(250.0);
        app.consoles.clear();
        let table = |name: &str| TableInfo {
            name: name.into(),
            kind: ObjectKind::Table,
            columns: vec![ColumnInfo { name: "id".into(), data_type: "int".into(), nullable: false, primary_key: true }],
        };
        for (id, name, driver, environment) in [
            (7, "orders-prod", Driver::Postgres, Environment::Production),
            (8, "orders-staging", Driver::Postgres, Environment::Staging),
            (9, "local", Driver::MySql, Environment::Development),
        ] {
            let mut cfg = ConnectionConfig::new(id, driver);
            cfg.name = name.into();
            cfg.database = "shop".into();
            cfg.environment = environment;
            let mut conn = super::super::ConnState::new(cfg);
            conn.status = super::super::ConnStatus::Connected;
            conn.databases = vec![DatabaseInfo {
                name: "shop".into(),
                current: true,
                schemas: Some(vec![SchemaInfo {
                    name: "public".into(),
                    tables: ["audit_log", "customers", "log", "orders", "tmp_import"].map(table).to_vec(),
                }]),
            }];
            app.conns.push(conn);
        }
        app.explorer.expanded.extend(["c:7".into(), "d:7:shop".into(), "s:7:shop:public".into(), "g:7:shop:public:t".into()]);
        let prod = "-- Close the orders paid before the migration\n\
                    UPDATE orders\n   SET status = 'closed'\n WHERE status = 'paid'\n   AND paid_at < '2026-10-01';";
        app.consoles.push(Console::new(1, "Close paid orders".into(), prod.into(), Some(7)));
        let cleanup = "-- Reset the staging copy before the next import\n\
                       DELETE FROM log;\nTRUNCATE TABLE audit_log;\nDROP TABLE IF EXISTS tmp_import;\n\
                       ALTER TABLE orders DROP COLUMN legacy_note;\nUPDATE orders SET status = 'new' WHERE id > 1000;";
        app.consoles.push(Console::new(2, "Staging cleanup".into(), cleanup.into(), Some(8)));
        app.consoles.push(Console::new(3, "Local".into(), "SELECT count(*) FROM orders;".into(), Some(9)));
        app.active = 0;
        app.next_console_id = 4;
        app
    }

    fn app_harness(app: DataCraftApp) -> egui_kittest::Harness<'static, DataCraftApp> {
        let dark = app.dark;
        let mut first = true;
        let mut frame = eframe::Frame::_new_kittest();
        let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(1440.0, 860.0)).build_ui_state(
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

    /// The documentation captures: environment badges, the dangerous statement prompt, the
    /// Production prompt for statements and grid saves, and the data source's Options tab
    /// (dark theme only, to keep the repository small).
    #[test]
    #[ignore = "Requires a GPU; renders the dangerous statement guard and connection environments without a database"]
    fn render_safety_screenshots() {
        use egui_kittest::kittest::Queryable;
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/safety-snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        let shot = |h: &mut egui_kittest::Harness<'static, DataCraftApp>, name: &str| {
            h.run_steps(3);
            h.render().unwrap().save(dir.join(name)).unwrap();
        };
        let mut h = app_harness(screenshot_app(true));
        assert!(h.query_by_label("Production").is_some());
        shot(&mut h, "production-console-dark.png");

        h.state_mut().run_console(0, true);
        assert!(h.state().safety.confirm.as_ref().is_some_and(|c| c.production.is_some() && c.dangerous.is_empty()));
        shot(&mut h, "production-confirm-dark.png");
        h.key_press(egui::Key::Escape);
        h.run_steps(2);
        assert!(h.state().safety.confirm.is_none() && !h.state().consoles[0].running);

        h.state_mut().active = 1;
        h.run_steps(2);
        h.state_mut().run_console(1, true);
        assert_eq!(h.state().safety.confirm.as_ref().map(|c| c.dangerous.len()), Some(4));
        shot(&mut h, "dangerous-confirm-dark.png");
        // A closed modal forgets its size before the next one opens.
        h.state_mut().safety.confirm = None;
        h.state_mut().active = 0;
        h.state_mut().consoles[0].text = "SELECT * FROM orders WHERE status = 'paid';".into();
        h.run_steps(3);
        h.state_mut().safety.confirm = h.state().changes_confirmation(7, 1);
        shot(&mut h, "production-grid-save-dark.png");
        h.state_mut().safety.confirm = None;

        let cfgs = h.state().conn_configs();
        h.state_mut().dialog = Some(crate::ui::connection_dialog::ConnectionDialog::edit(7, &cfgs));
        h.run_steps(3);
        h.query_all_by_label("Options").last().expect("Options tab").click();
        h.hover_at(egui::pos2(-10.0, -10.0));
        shot(&mut h, "data-source-options-dark.png");
    }
}
