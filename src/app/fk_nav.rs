//! Foreign-key navigation between table tabs: keys cached per table, jumps to a parent row or
//! to the rows referencing one ("Referenced by…"), and back / forward history of those jumps.

use super::*;
use crate::db::fk_nav::{COUNT_LIMIT, Probe, TableKeys, key_filter};
use crate::db::subset::ForeignKey;
use crate::db::{Cell, ObjectKind};
use crate::ui::filter_sort::{Condition, FilterSort, Node, Operator};
use crate::ui::fk_nav::{Jump, Links, column_names};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

const HISTORY: usize = 50;

type TableId = (ConnId, String, String, String);

/// The keys of one table, as cached.
enum Keys {
    /// Request `id` is reading them: an older reply (e.g. from before a refresh) is ignored.
    Loading(u64),
    Ready(Arc<TableKeys>),
    /// Reading them failed (reported once); a reload or schema refresh tries again.
    Failed,
}

#[derive(Default)]
pub(super) struct State {
    /// Keys per (connection, database, schema, table). Dropped when the schema is refreshed, the
    /// table is reloaded or the connection closes.
    cache: HashMap<TableId, Keys>,
    back: Vec<Place>,
    forward: Vec<Place>,
    dialog: Option<Dialog>,
    next_request: u64,
    pending: HashMap<u64, (String, String)>,
    popup: Option<(u64, egui::Pos2, Vec<crate::ui::fk_nav::Target>, usize)>,
}

impl State {
    pub(super) fn is_open(&self) -> bool {
        self.dialog.is_some() || self.popup.is_some()
    }
}

/// Where a jump started or landed: a tab and, for a table tab, its table and filter.
#[derive(Clone)]
struct Place {
    console: u64,
    table: Option<Filtered>,
}

/// A table and the filter to show it with.
#[derive(Clone)]
struct Filtered {
    conn: ConnId,
    db: String,
    schema: String,
    table: String,
    where_clause: String,
    bound_filter: Option<crate::db::fk_nav::BoundFilter>,
    order_by: String,
    filter: FilterSort,
}

/// "Referenced by…" for one row.
struct Dialog {
    id: u64,
    conn: ConnId,
    db: String,
    driver: Driver,
    subtitle: String,
    origin: Place,
    children: Vec<Child>,
    /// Children counted, in the order of the counts.
    probed: Vec<usize>,
    cancel: Arc<AtomicBool>,
    pending: bool,
    error: Option<String>,
}

struct Child {
    key: ForeignKey,
    /// The row's referenced values (`None` when the result lacks a referenced column).
    values: Option<Vec<Cell>>,
    count: Option<Result<u64, String>>,
}

/// The table filtered to the rows whose `columns` hold `values`, as builder conditions.
fn filtered(driver: Driver, conn: ConnId, db: &str, schema: &str, table: &str, columns: &[String], values: &[Cell]) -> Option<Filtered> {
    filtered_rows(driver, conn, db, schema, table, columns, &[values.to_vec()])
}

fn filtered_rows(driver: Driver, conn: ConnId, db: &str, schema: &str, table: &str, columns: &[String], values: &[Vec<Cell>]) -> Option<Filtered> {
    use crate::ui::filter_sort::{Connector, FilterRow, Group};
    let bound_filter = crate::db::fk_nav::bound_filter_rows(driver, columns, values)?;
    let where_clause = bound_filter.display.clone();
    let conditions = |values: &[String]| columns.iter().zip(values).map(|(column, value)| {
        Node::Condition(Condition { enabled: true, column: column.clone(), operator: Operator::Equal, value: value.clone(), second: String::new() }).into()
    }).collect::<Vec<FilterRow>>();
    let mut filter = FilterSort::default();
    // Reconstruct only normalized, non-NULL tuples actually used by the bound query.
    filter.filter.children = if bound_filter.parameters.len() == columns.len() {
        conditions(&bound_filter.parameters)
    } else {
        bound_filter.parameters.chunks(columns.len()).map(|values| FilterRow {
            node: Node::Group(Group { children: conditions(values) }), connector: Connector::Or,
        }).collect()
    };
    Some(Filtered { conn, db: db.into(), schema: schema.into(), table: table.into(), where_clause, bound_filter: Some(bound_filter), order_by: String::new(), filter })
}

/// Which references the catalog readers of `driver` cannot see.
fn scope_note(driver: Driver) -> Option<&'static str> {
    match driver {
        Driver::Oracle => Some(tr("Only references from tables of the same owner are listed.")),
        Driver::MySql | Driver::MariaDb | Driver::Snowflake => Some(tr("Only references from tables of the same database are listed.")),
        Driver::Sqlite => Some(tr("Only references between tables of the main schema are listed.")),
        _ => None,
    }
}

impl DataCraftApp {
    /// Follows the declared keys of a tab's new results, reading a table's keys once.
    pub(super) fn attach_key_links(&mut self, console: u64) {
        let Some(idx) = self.consoles.iter().position(|c| c.id == console) else { return };
        let c = &self.consoles[idx];
        let Some(conn) = c.conn else { return };
        let Some(cs) = self.conn(conn).filter(|cs| cs.status == ConnStatus::Connected && crate::db::fk_nav::supported(cs.cfg.driver)) else { return };
        let Some(db) = cs.resolve_db(c.db.as_deref()).map(str::to_string) else { return };
        let mut found = Vec::new();
        for (i, v) in c.results.iter().enumerate().filter(|(_, v)| !v.links_checked && !v.rs.is_command()) {
            let source = match &c.table {
                Some(t) => {
                    let names = match self.table_columns(conn, Some(&db), &t.schema, &t.table) {
                        Some(columns) => column_names(&v.rs.columns, columns),
                        None => v.rs.columns.iter().cloned().map(Some).collect(),
                    };
                    Some((t.schema.clone(), t.table.clone(), names))
                }
                // Console results: only a simple query over one known table.
                None => cs
                    .schemas(Some(&db))
                    .and_then(|schemas| crate::ui::editing::query_table(&v.rs.statement, schemas).ok())
                    .filter(|(_, t)| t.kind == ObjectKind::Table)
                    .map(|(s, t)| (s.name.clone(), t.name.clone(), column_names(&v.rs.columns, &t.columns))),
            };
            found.push((i, source));
        }
        for (i, source) in found {
            let view = &mut self.consoles[idx].results[i];
            let Some((schema, table, names)) = source else {
                view.links_checked = true;
                continue;
            };
            let id = (conn, db.clone(), schema.clone(), table.clone());
            match self.fk_nav.cache.get(&id) {
                Some(Keys::Ready(keys)) => {
                    view.links = self.conns.iter().find(|c| c.cfg.id == conn).and_then(|c| c.schemas(Some(&db))).and_then(|schemas| Links::with_schema(keys, &names, schemas, &schema, &table)).or_else(|| Links::new(keys, &names)).map(Arc::new);
                    view.links_checked = true;
                }
                Some(Keys::Failed) => view.links_checked = true,
                Some(Keys::Loading(_)) => {}
                None => {
                    self.fk_nav.next_request += 1;
                    let request_id = self.fk_nav.next_request;
                    self.fk_nav.cache.insert(id, Keys::Loading(request_id));
                    self.worker.send(Request::ForeignKeys { conn, db: db.clone(), schema, table, request_id });
                }
            }
        }
    }

    pub(super) fn foreign_keys_received(&mut self, conn: ConnId, db: String, schema: String, table: String, request_id: u64, result: Result<TableKeys, String>) {
        // Only the newest request of the table answers: a reply from before a refresh is stale.
        let Some(entry) = self.fk_nav.cache.get_mut(&(conn, db, schema.clone(), table.clone())).filter(|e| matches!(e, Keys::Loading(id) if *id == request_id)) else { return };
        match result {
            Ok(keys) => *entry = Keys::Ready(Arc::new(keys)),
            Err(error) => {
                *entry = Keys::Failed;
                self.error(trf("Could not read the foreign keys of {table}: {error}", &[("table", &format!("{schema}.{table}")), ("error", &error)]));
            }
        }
        let ids: Vec<u64> = self.consoles.iter().filter(|c| c.conn == Some(conn)).map(|c| c.id).collect();
        for id in ids {
            self.attach_key_links(id);
        }
    }

    /// Reads the keys again (schema refresh, table reload): `table` = (db, schema, table), or all.
    pub(super) fn forget_keys(&mut self, conn: ConnId, table: Option<(&str, &str, &str)>) {
        let hit = |id: &TableId| id.0 == conn && table.is_none_or(|(d, s, t)| id.1 == d && id.2 == s && id.3 == t);
        self.fk_nav.cache.retain(|id, _| !hit(id));
        let ids: Vec<u64> = self.consoles.iter().filter(|c| c.conn == Some(conn)).map(|c| c.id).collect();
        for c in self.consoles.iter_mut().filter(|c| c.conn == Some(conn)) {
            for v in &mut c.results {
                v.links_checked = false;
                v.links = None;
            }
        }
        for id in ids {
            self.attach_key_links(id);
        }
    }

    fn place(&self, idx: usize) -> Option<Place> {
        let c = self.consoles.get(idx)?;
        let table = c.table.as_ref().and_then(|t| {
            let conn = c.conn?;
            let db = self.conn(conn)?.resolve_db(c.db.as_deref())?.to_string();
            let filter = t.filter_sort.clone();
            Some(Filtered { conn, db, schema: t.schema.clone(), table: t.table.clone(), where_clause: t.where_clause.clone(), bound_filter: t.bound_filter.clone(), order_by: t.order_by.clone(), filter })
        });
        Some(Place { console: c.id, table })
    }

    pub(super) fn follow_key(&mut self, idx: usize, jump: Jump) {
        let Some(origin) = self.place(idx) else { return };
        let Some(conn) = self.consoles[idx].conn else { return };
        let Some(cs) = self.conn(conn) else { return };
        let (driver, db) = (cs.cfg.driver, cs.resolve_db(self.consoles[idx].db.as_deref()).unwrap_or_default().to_string());
        match jump {
            Jump::Choose { targets, anchor } => { self.fk_nav.popup = Some((self.consoles[idx].id, anchor, targets, 0)); }
            Jump::Null => self.info("Cannot navigate: source value is NULL"),
            Jump::Rows { key, values } => {
                if let Some(target) = filtered_rows(driver, conn, &db, &key.ref_schema, &key.ref_table, &key.ref_columns, &values) {
                    self.go(origin, &target);
                } else { self.info("Cannot navigate: source value is NULL"); }
            }
            Jump::Parent { key, values } => {
                if values.iter().any(Option::is_none) { self.info("Cannot navigate: source value is NULL"); return; }
                if let Some(target) = filtered(driver, conn, &db, &key.ref_schema, &key.ref_table, &key.ref_columns, &values) {
                    self.go(origin, &target);
                }
            }
            Jump::Children { keys } => {
                if let Some(old) = self.fk_nav.dialog.take() {
                    old.cancel.store(true, Ordering::Relaxed);
                }
                // `shop.orders · id = 1004`: the row, by the first key's referenced columns.
                let row = keys.iter().find_map(|(k, values)| {
                    let pairs = k.ref_columns.iter().zip(values.as_ref()?).map(|(c, v)| format!("{c} = {}", v.as_deref().unwrap_or("NULL")));
                    Some(format!("{}.{} · {}", k.ref_schema, k.ref_table, pairs.collect::<Vec<_>>().join(", ")))
                });
                let parent = row.unwrap_or_default();
                let children: Vec<Child> = keys.into_iter().map(|(key, values)| Child { key, values, count: None }).collect();
                let mut probed = Vec::new();
                let mut probes = Vec::new();
                for (i, child) in children.iter().enumerate() {
                    if let Some(filter) = child.values.as_ref().and_then(|values| key_filter(driver, &child.key.columns, values)) {
                        probed.push(i);
                        probes.push(Probe { schema: child.key.schema.clone(), table: child.key.table.clone(), filter });
                    }
                }
                self.fk_nav.next_request += 1;
                let (id, cancel) = (self.fk_nav.next_request, Arc::new(AtomicBool::new(false)));
                let pending = !probes.is_empty();
                if pending {
                    self.worker.send(Request::ReferenceCounts { conn, db: db.clone(), request_id: id, probes, cancel: Arc::clone(&cancel) });
                }
                self.fk_nav.dialog = Some(Dialog { id, conn, db, driver, subtitle: parent, origin, children, probed, cancel, pending, error: None });
            }
        }
    }

    pub(super) fn reference_counts_received(&mut self, request_id: u64, result: Result<Vec<Result<u64, String>>, String>) {
        let Some(dialog) = self.fk_nav.dialog.as_mut().filter(|d| d.id == request_id) else { return };
        dialog.pending = false;
        match result {
            Ok(counts) => {
                for (&i, count) in dialog.probed.iter().zip(counts) {
                    dialog.children[i].count = Some(count);
                }
            }
            Err(error) => dialog.error = Some(error),
        }
    }

    /// Shows `target` (in a clean tab of that table, or a new one) and records where it came from.
    fn go(&mut self, origin: Place, target: &Filtered) {
        if let Some(idx) = self.open_filtered(target, None) {
            let value = target.bound_filter.as_ref().map(|b| b.parameters.join(", ")).unwrap_or_default();
            self.fk_nav.pending.insert(self.consoles[idx].id, (target.table.clone(), value));
            self.fk_nav.back.push(origin);
            if self.fk_nav.back.len() > HISTORY {
                self.fk_nav.back.remove(0);
            }
            self.fk_nav.forward.clear();
        }
    }

    /// Prefers tab `console`, then any tab of the table without pending changes; else a new tab.
    fn open_filtered(&mut self, target: &Filtered, console: Option<u64>) -> Option<usize> {
        self.object_browser.active = false;
        let default_db = self.conn(target.conn).and_then(|c| c.default_db()).map(str::to_string);
        let reusable = |c: &Console| {
            c.conn == Some(target.conn)
                && c.db.as_ref().or(default_db.as_ref()) == Some(&target.db)
                && c.table.as_ref().is_some_and(|t| t.schema == target.schema && t.table == target.table)
                && !c.has_pending_edits()
                && !c.is_busy()
        };
        let found = console.and_then(|id| self.consoles.iter().position(|c| c.id == id && reusable(c))).or_else(|| self.consoles.iter().position(reusable));
        let idx = match found {
            Some(idx) => idx,
            None => {
                let id = self.alloc_console_id();
                let mut tab = Console::new_table(id, target.conn, Some(target.db.clone()), target.schema.clone(), target.table.clone());
                if let Some(t) = tab.table.as_mut() {
                    t.save_on_row_change = self.save_on_row_change;
                }
                self.consoles.push(tab);
                self.consoles.len() - 1
            }
        };
        let t = self.consoles[idx].table.as_mut()?;
        t.where_clause.clone_from(&target.where_clause);
        t.bound_filter = target.bound_filter.clone();
        t.order_by.clone_from(&target.order_by);
        t.filter_sort = target.filter.clone();
        t.offset = 0;
        t.page_input_reset();
        self.active = idx;
        self.reload_table(idx);
        Some(idx)
    }

    /// Goes back to (or forward to) a recorded place; closed tabs reopen with their filter.
    pub(super) fn key_history(&mut self, forward: bool) {
        let Some(current) = self.place(self.active) else { return };
        loop {
            let next = if forward { self.fk_nav.forward.pop() } else { self.fk_nav.back.pop() };
            let Some(place) = next else { return };
            let open = self.consoles.iter().position(|c| c.id == place.console);
            let restored = match (&place.table, open) {
                // Pending changes stay as they are: the tab is only activated.
                (_, Some(idx)) if self.consoles[idx].has_pending_edits() || self.consoles[idx].is_busy() => {
                    self.active = idx;
                    true
                }
                (Some(table), _) => self.open_filtered(table, Some(place.console)).is_some(),
                (None, Some(idx)) => {
                    self.active = idx;
                    true
                }
                (None, None) => false,
            };
            if restored {
                if forward { self.fk_nav.back.push(current) } else { self.fk_nav.forward.push(current) }
                return;
            }
        }
    }

    pub(super) fn key_history_flags(&self) -> [bool; 2] {
        [!self.fk_nav.back.is_empty(), !self.fk_nav.forward.is_empty()]
    }

    pub(super) fn navigation_finished(&mut self, console: u64, succeeded: bool) {
        let Some((table, value)) = self.fk_nav.pending.remove(&console) else { return };
        if !succeeded { return; }
        let found = self.console_by_id(console).and_then(|c| c.results.first_mut()).is_some_and(|v| v.highlight_first_row());
        if !found { self.info(format!("No matching row in `{table}` for `{value}`")); }
    }

    pub(super) fn references_dialog(&mut self, ctx: &egui::Context) {
        if let Some((console, anchor, targets, mut selected)) = self.fk_nav.popup.clone() {
            if targets.is_empty() { self.fk_nav.popup = None; return; }
            ctx.input_mut(|i| {
                if i.consume_key(egui::Modifiers::NONE, Key::ArrowDown) { selected = (selected + 1) % targets.len(); }
                if i.consume_key(egui::Modifiers::NONE, Key::ArrowUp) { selected = (selected + targets.len() - 1) % targets.len(); }
            });
            if let Some(popup) = self.fk_nav.popup.as_mut() { popup.3 = selected; }
            let id = egui::Id::new("navigation_targets");
            let area = egui::Area::new(id).kind(egui::UiKind::Modal).order(egui::Order::Foreground).fixed_pos(anchor).constrain(true);
            let mut chosen = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Enter)).then(|| targets[selected].clone().jump());
            let popup = egui::Modal::new(id).area(area).backdrop_color(egui::Color32::TRANSPARENT).show(ctx, |ui| {
                ui.label(RichText::new(tr("Go to referenced row")).strong());
                for (i, target) in targets.into_iter().enumerate() {
                    let label = target.label();
                    if ui.selectable_label(i == selected, label).clicked() { chosen = Some(target.jump()); }
                }
            });
            if popup.should_close() || chosen.is_some() { self.fk_nav.popup = None; }
            if let Some(jump) = chosen && let Some(idx) = self.consoles.iter().position(|c| c.id == console) { self.follow_key(idx, jump); }
        }
        let Some(dialog) = self.fk_nav.dialog.as_ref() else { return };
        let p = palette(ctx.global_style().visuals.dark_mode);
        let (mut close, mut open) = (false, None);
        crate::ui::workflow_dialog::show(ctx, egui::Id::new("fk_referenced_by"), tr("Referenced by"), &dialog.subtitle, vec2(640.0, 360.0), |root, regions| {
            let mut body = regions.body_ui(root);
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(&mut body, |ui| {
                egui::Grid::new("fk_children").num_columns(4).striped(true).spacing(vec2(18.0, 8.0)).show(ui, |ui| {
                    for title in [tr("Table"), tr("Columns"), tr("Rows"), ""] {
                        ui.label(RichText::new(title).strong());
                    }
                    ui.end_row();
                    for (i, child) in dialog.children.iter().enumerate() {
                        ui.label(format!("{}.{}", child.key.schema, child.key.table));
                        ui.label(RichText::new(child.key.columns.join(", ")).color(p.muted));
                        match &child.count {
                            Some(Ok(n)) if *n > COUNT_LIMIT => ui.label(format!("{COUNT_LIMIT}+")),
                            Some(Ok(n)) => ui.label(n.to_string()),
                            Some(Err(error)) => ui.label(RichText::new("—").color(p.error)).on_hover_text(error),
                            None if child.values.is_none() => ui.label(RichText::new("—").color(p.muted)).on_hover_text(tr("The result does not include the referenced columns")),
                            None if dialog.pending => ui.spinner(),
                            None => ui.label(RichText::new("—").color(p.muted)),
                        };
                        let can_open = child.count != Some(Ok(0)) && child.values.as_ref().is_some_and(|v| key_filter(dialog.driver, &child.key.columns, v).is_some());
                        if ui.add_enabled(can_open, egui::Button::new(tr("Open rows"))).clicked() {
                            open = Some(i);
                        }
                        ui.end_row();
                    }
                });
                if let Some(error) = &dialog.error {
                    ui.add(egui::Label::new(RichText::new(error).color(p.error)).wrap());
                }
            });
            regions.footer_ui(root).with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(tr("Close")).clicked() {
                    close = true;
                }
                if dialog.pending {
                    ui.label(RichText::new(tr("Counting referencing rows…")).color(p.muted));
                } else if let Some(note) = scope_note(dialog.driver) {
                    ui.add(egui::Label::new(RichText::new(note).small().color(p.muted)).wrap());
                }
            });
        });
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            close = true;
        }
        if close || open.is_some() {
            self.close_references(open);
        }
    }

    /// Closes "Referenced by…" (cancelling its counts), opening the rows of child `open`.
    fn close_references(&mut self, open: Option<usize>) {
        let Some(dialog) = self.fk_nav.dialog.take() else { return };
        dialog.cancel.store(true, Ordering::Relaxed);
        let target = open.and_then(|i| {
            let child = dialog.children.get(i)?;
            filtered(dialog.driver, dialog.conn, &dialog.db, &child.key.schema, &child.key.table, &child.key.columns, child.values.as_ref()?)
        });
        if let Some(target) = target {
            self.go(dialog.origin, &target);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pump(app: &mut DataCraftApp, done: impl Fn(&DataCraftApp) -> bool) {
        let deadline = Instant::now() + std::time::Duration::from_secs(20);
        while !done(app) {
            assert!(Instant::now() < deadline, "timed out waiting for the worker");
            app.handle_responses();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn tab(app: &DataCraftApp, table: &str) -> usize {
        app.consoles.iter().position(|c| c.table.as_ref().is_some_and(|t| t.table == table)).unwrap()
    }

    /// The tab of `table` finished loading and its keys were looked up.
    fn loaded(app: &DataCraftApp, table: &str) -> bool {
        app.consoles.iter().any(|c| !c.is_busy() && c.table.as_ref().is_some_and(|t| t.table == table) && c.results.last().is_some_and(|v| v.links_checked))
    }

    #[test]
    fn sqlite_multi_selection_matches_complete_keys_in_both_directions() {
        let path = std::env::temp_dir().join(format!("datacraft-fk-multi-{}.db", uuid::Uuid::new_v4()));
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE shops(code TEXT,region TEXT,PRIMARY KEY(code,region));
            CREATE TABLE orders(id INTEGER PRIMARY KEY,shop TEXT,region TEXT,FOREIGN KEY(shop,region) REFERENCES shops(code,region));
            INSERT INTO shops VALUES('A','north'),('B','south'),('A','south'),('B','north');
            INSERT INTO orders VALUES(1,'A','north'),(2,'B','south'),(3,'A','north'),(4,'A',NULL),(5,'A','south'),(6,'B','north');").unwrap();
        drop(db);
        let mut app = super::super::tests::tabs_app();
        let mut cfg = ConnectionConfig::new(7, Driver::Sqlite);
        cfg.file_path = path.to_string_lossy().into_owned();
        app.conns.push(ConnState::new(cfg));
        app.connect(7);
        pump(&mut app, |app| app.conn(7).is_some_and(|c| c.status == ConnStatus::Connected));
        app.open_table(&TableRef { conn: 7, db: "main".into(), schema: "main".into(), table: "orders".into() });
        pump(&mut app, |app| loaded(app, "orders"));
        let orders = tab(&app, "orders");
        let view = app.consoles[orders].results.last().unwrap();
        let links = view.links.as_ref().unwrap();
        let targets = links.targets_for_cells(view.rs.rows[..4].iter().flat_map(|r| [(1, r.as_slice()), (2, r.as_slice())]));
        assert_eq!(targets.len(), 1);
        app.follow_key(orders, targets[0].clone().jump());
        pump(&mut app, |app| loaded(app, "shops"));
        let shops = tab(&app, "shops");
        let view = app.consoles[shops].results.last().unwrap();
        assert_eq!(view.rs.rows, vec![vec![Some("A".into()), Some("north".into())], vec![Some("B".into()), Some("south".into())]]);
        let table = app.consoles[shops].table.as_ref().unwrap();
        assert_eq!(table.bound_filter.as_ref().unwrap().parameters, ["A", "north", "B", "south"]);
        let columns = [ColumnInfo { name: "code".into(), data_type: "TEXT".into(), nullable: false, primary_key: true }, ColumnInfo { name: "region".into(), data_type: "TEXT".into(), nullable: false, primary_key: true }];
        assert_eq!(table.filter_sort.sql(Driver::Sqlite, &columns).unwrap().0, table.where_clause);
        let reverse = view.links.as_ref().unwrap().targets_for_cells(view.rs.rows.iter().map(|r| (0, r.as_slice())));
        app.follow_key(shops, reverse[0].clone().jump());
        pump(&mut app, |app| loaded(app, "orders"));
        assert_eq!(app.consoles[orders].results[0].rs.rows.iter().map(|r| r[0].as_deref().unwrap()).collect::<Vec<_>>(), ["1", "2", "3"]);
        app.key_history(false);
        pump(&mut app, |app| loaded(app, "shops"));
        assert_eq!(app.consoles[shops].results[0].rs.rows.len(), 2);
        app.worker.send(Request::Disconnect(7));
        drop(app);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn sqlite_jumps_follow_composite_keys_count_children_and_go_back() {
        let path = std::env::temp_dir().join(format!("datacraft-fk-jumps-{}.db", uuid::Uuid::new_v4()));
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE TABLE shops(code TEXT,region TEXT,name TEXT,PRIMARY KEY(code,region));
             CREATE TABLE orders(id INTEGER PRIMARY KEY,shop TEXT,region TEXT,note TEXT,FOREIGN KEY(shop,region) REFERENCES shops(code,region));
             INSERT INTO shops VALUES('N''1','north','O''Brien'),('N''1','south','Other');
             INSERT INTO orders VALUES(1,'N''1','north','a'),(2,'N''1','north','b'),(3,NULL,'north','c'),(4,'N''1','south','d');",
        )
        .unwrap();
        drop(db);
        let mut app = super::super::tests::tabs_app();
        let mut cfg = ConnectionConfig::new(7, Driver::Sqlite);
        cfg.file_path = path.to_string_lossy().into_owned();
        app.conns.push(ConnState::new(cfg));
        app.connect(7);
        pump(&mut app, |app| app.conn(7).is_some_and(|c| c.status == ConnStatus::Connected));
        app.open_table(&TableRef { conn: 7, db: "main".into(), schema: "main".into(), table: "orders".into() });
        pump(&mut app, |app| loaded(app, "orders"));

        let orders = tab(&app, "orders");
        let view = app.consoles[orders].results.last().unwrap();
        let links = view.links.clone().expect("orders follow shops");
        let shop = view.rs.columns.iter().position(|c| c == "shop").unwrap();
        let rows = view.rs.rows.clone();
        assert!(links.link_at(shop, &rows[2]).is_none(), "a NULL key is not a link");
        let key = links.link_at(shop, &rows[0]).unwrap();
        app.follow_key(orders, links.parent(key, &rows[0]).unwrap());
        pump(&mut app, |app| loaded(app, "shops"));

        let shops = tab(&app, "shops");
        let t = app.consoles[shops].table.as_ref().unwrap();
        assert_eq!(t.where_clause, "(\"code\" = 'N''1' AND \"region\" = 'north')");
        assert_eq!(t.filter_sort.filter.children.len(), 2);
        let view = app.consoles[shops].results.last().unwrap();
        assert_eq!(view.rs.rows.len(), 1);
        assert_eq!(view.rs.rows[0][2].as_deref(), Some("O'Brien"));
        assert_eq!(view.row_position(), Some((1, 1)));
        assert!(view.rs.statement.contains('?'));
        assert_eq!(t.bound_filter.as_ref().unwrap().parameters, ["N'1", "north"]);
        let children = view.links.as_ref().unwrap().children(&view.rs.rows[0]).unwrap();
        app.follow_key(shops, children);
        pump(&mut app, |app| app.fk_nav.dialog.as_ref().is_some_and(|d| !d.pending));
        assert_eq!(app.fk_nav.dialog.as_ref().unwrap().children[0].count, Some(Ok(2)));
        assert_eq!(app.fk_nav.dialog.as_ref().unwrap().subtitle, "main.shops · code = N'1, region = north");
        let cancel = Arc::clone(&app.fk_nav.dialog.as_ref().unwrap().cancel);
        app.close_references(Some(0));
        assert!(cancel.load(Ordering::Relaxed) && app.fk_nav.dialog.is_none());
        assert_eq!(app.active, orders, "the clean orders tab is reused");
        pump(&mut app, |app| loaded(app, "orders"));
        assert_eq!(app.consoles[orders].results.last().unwrap().rs.rows.iter().map(|r| r[0].clone().unwrap()).collect::<Vec<_>>(), ["1", "2"]);

        app.key_history(false);
        assert_eq!(app.active, shops);
        pump(&mut app, |app| loaded(app, "shops"));
        app.key_history(false);
        assert_eq!((app.active, app.consoles[orders].table.as_ref().unwrap().where_clause.as_str()), (orders, ""));
        pump(&mut app, |app| loaded(app, "orders"));
        assert_eq!(app.consoles[orders].results.last().unwrap().rs.rows.len(), 4);
        assert_eq!(app.key_history_flags(), [false, true]);
        app.key_history(true);
        assert_eq!(app.active, shops);
        pump(&mut app, |app| loaded(app, "shops"));

        // Console results: a simple query over one table maps its columns; a join does not. A
        // Manual transaction console keeps read-only grids, but its keys can still be followed.
        app.consoles[0].conn = Some(7);
        app.consoles[0].manual_tx = true;
        app.run_statements(0, vec!["SELECT ID, region, shop FROM orders WHERE id = 1".into(), "SELECT o.id, o.shop FROM orders o JOIN shops s ON s.code = o.shop".into()]);
        pump(&mut app, |app| !app.consoles[0].is_busy() && app.consoles[0].results.len() == 2 && app.consoles[0].results.iter().all(|v| v.links_checked));
        let simple = app.consoles[0].results[0].links.clone().expect("a simple query follows its keys");
        assert_eq!(simple.outgoing[0].1, [2, 1]);
        assert!(app.consoles[0].results[0].edit.is_none() && app.consoles[0].results[1].links.is_none());
        app.follow_key(0, simple.parent(0, &app.consoles[0].results[0].rs.rows[0]).unwrap());
        assert_eq!((app.active, app.consoles[0].manual_tx), (shops, true), "the jump opens the table tab, the console keeps its transaction");
        pump(&mut app, |app| loaded(app, "shops"));
        app.key_history(false);
        assert_eq!(app.active, 0);

        // Dangling keys report an empty target; NULL never starts a query.
        let dangling = Jump::Parent { key: simple.outgoing[0].0.clone(), values: vec![Some("missing".into()), Some("north".into())] };
        app.follow_key(orders, dangling);
        pump(&mut app, |app| loaded(app, "shops"));
        assert!(app.consoles[shops].results[0].rs.rows.is_empty());
        assert_eq!(app.status_msg.as_ref().unwrap().0, "No matching row in `shops` for `missing, north`");
        app.follow_key(orders, Jump::Null);
        assert_eq!(app.status_msg.as_ref().unwrap().0, "Cannot navigate: source value is NULL");
        assert!(!app.consoles[shops].is_busy());
        app.follow_key(orders, links.parent(key, &rows[0]).unwrap());
        pump(&mut app, |app| loaded(app, "shops"));

        app.forget_keys(7, Some(("main", "main", "shops")));
        let shops_id: TableId = (7, "main".into(), "main".into(), "shops".into());
        let Some(&Keys::Loading(stale)) = app.fk_nav.cache.get(&shops_id) else { panic!("reload reads the keys again") };
        // A reply to an older request of the table is ignored; a failure is reported once.
        app.foreign_keys_received(7, "main".into(), "main".into(), "shops".into(), stale - 1, Ok(TableKeys::default()));
        assert!(matches!(app.fk_nav.cache.get(&shops_id), Some(Keys::Loading(id)) if *id == stale));
        let ghost: TableId = (7, "main".into(), "main".into(), "ghost".into());
        app.fk_nav.cache.insert(ghost.clone(), Keys::Loading(999));
        app.foreign_keys_received(7, "main".into(), "main".into(), "ghost".into(), 999, Err("timed out".into()));
        assert!(matches!(app.fk_nav.cache.get(&ghost), Some(Keys::Failed)));
        assert!(app.status_msg.as_ref().is_some_and(|(m, error)| *error && m.contains("main.ghost") && m.contains("timed out")));
        pump(&mut app, |app| loaded(app, "shops") && app.consoles[tab(app, "shops")].results.last().unwrap().links.is_some());
        app.worker.send(Request::Disconnect(7));
        pump(&mut app, |app| app.conn(7).is_some_and(|c| c.status == ConnStatus::Disconnected));
        std::fs::remove_file(path).unwrap();
    }
}

/// Documentation captures of foreign-key navigation, rendered from the real app UI with
/// synthetic data (no database).
#[cfg(all(test, target_os = "windows"))]
mod screenshots {
    use super::*;
    use crate::db::object_info::ObjectInfo;
    use crate::db::{ResultSet, TableInfo};
    use egui_kittest::Harness;

    fn columns(names: &[(&str, &str)]) -> Vec<ColumnInfo> {
        names.iter().map(|(name, ty)| ColumnInfo { name: (*name).into(), data_type: (*ty).into(), nullable: *name == "customer_id", primary_key: *name == "id" }).collect()
    }

    fn key(table: &str, column: &str, ref_table: &str) -> ForeignKey {
        ForeignKey { schema: "shop".into(), table: table.into(), columns: vec![column.into()], ref_schema: "shop".into(), ref_table: ref_table.into(), ref_columns: vec!["id".into()] }
    }

    fn table_tab(id: u64, table: &str, rs: ResultSet, keys: &TableKeys, rows: u64) -> Console {
        let mut tab = Console::new_table(id, 7, Some("shop".into()), "shop".into(), table.into());
        let names: Vec<_> = rs.columns.iter().cloned().map(Some).collect();
        let mut view = ResultView::new(rs, 300 + id);
        view.links = Links::new(keys, &names).map(Arc::new);
        view.links_checked = true;
        tab.results.push(view);
        let t = tab.table.as_mut().unwrap();
        t.loaded = true;
        t.info_requested = true;
        t.info = Some(Ok(ObjectInfo { rows: Some(rows), engine: Some("InnoDB".into()), collation: Some("utf8mb4_unicode_ci".into()), ..Default::default() }));
        tab
    }

    fn fixture() -> DataCraftApp {
        let mut app = super::super::tests::tabs_app();
        app.demo_explorer_w = Some(230.0);
        app.consoles.clear();
        let orders_cols = columns(&[("id", "int"), ("customer_id", "int"), ("status", "varchar(20)"), ("total", "decimal(10,2)"), ("created_at", "datetime")]);
        let customers_cols = columns(&[("id", "int"), ("name", "varchar(120)"), ("email", "varchar(160)"), ("country", "char(2)")]);
        let child = columns(&[("id", "int"), ("order_id", "int")]);
        let tables = [("audit_events", &child), ("customers", &customers_cols), ("order_items", &child), ("orders", &orders_cols), ("payments", &child), ("shipments", &child)]
            .into_iter()
            .map(|(name, columns)| TableInfo { name: name.into(), kind: ObjectKind::Table, columns: columns.clone() })
            .collect();
        let mut cfg = ConnectionConfig::new(7, Driver::MariaDb);
        cfg.name = "Development".into();
        cfg.database = "shop".into();
        let mut conn = ConnState::new(cfg);
        conn.status = ConnStatus::Connected;
        conn.databases = vec![DatabaseInfo { name: "shop".into(), current: true, schemas: Some(vec![SchemaInfo { name: "shop".into(), tables }]) }];
        app.conns.push(conn);
        app.explorer.expanded.extend(["c:7".into(), "d:7:shop".into()]);

        let orders_keys = TableKeys {
            outgoing: vec![key("orders", "customer_id", "customers")],
            incoming: vec![key("audit_events", "order_id", "orders"), key("order_items", "order_id", "orders"), key("payments", "order_id", "orders"), key("shipments", "order_id", "orders")],
        };
        let orders = ResultSet {
            columns: orders_cols.iter().map(|c| c.name.clone()).collect(),
            rows: (0..40)
                .map(|i: usize| {
                    let customer = (i % 9 != 4).then(|| ((i * 5) % 8 + 101).to_string());
                    let status = ["paid", "shipped", "pending", "paid"][i % 4];
                    vec![Some((1001 + i).to_string()), customer, Some(status.into()), Some(format!("{}.{:02}", 40 + i * 17 % 300, i * 7 % 100)), Some(format!("2026-10-{:02} 1{}:{:02}:00", 1 + i % 28, i % 10, i * 13 % 60))]
                })
                .collect(),
            elapsed: std::time::Duration::from_millis(18),
            ..Default::default()
        };
        app.consoles.push(table_tab(1, "orders", orders, &orders_keys, 40));
        let customer_keys = TableKeys { outgoing: Vec::new(), incoming: vec![key("orders", "customer_id", "customers")] };
        let customer = ResultSet {
            columns: customers_cols.iter().map(|c| c.name.clone()).collect(),
            rows: vec![vec![Some("103".into()), Some("Alan Turing".into()), Some("alan.turing@example.com".into()), Some("GB".into())]],
            elapsed: std::time::Duration::from_millis(4),
            ..Default::default()
        };
        let mut tab = table_tab(2, "customers", customer, &customer_keys, 8);
        let target = filtered(Driver::MariaDb, 7, "shop", "shop", "customers", &["id".into()], &[Some("103".into())]).unwrap();
        let t = tab.table.as_mut().unwrap();
        t.where_clause = target.where_clause;
        t.filter_sort = target.filter;
        app.consoles.push(tab);
        app.next_console_id = 3;
        app.active = 0;
        app.object_browser.active = false;
        app
    }

    fn harness() -> Harness<'static, DataCraftApp> {
        let mut first = true;
        let mut frame = eframe::Frame::_new_kittest();
        let mut harness = Harness::builder().with_size(vec2(1440.0, 820.0)).build_ui_state(
            move |ui, app| {
                if std::mem::take(&mut first) {
                    theme::install_fonts(ui.ctx());
                    theme::apply(ui.ctx(), true);
                    return;
                }
                let bounds = ui.ctx().content_rect();
                let mut root = ui.new_child(egui::UiBuilder::new().max_rect(bounds));
                root.set_clip_rect(bounds);
                eframe::App::ui(app, &mut root, &mut frame);
            },
            fixture(),
        );
        harness.run_steps(4);
        harness
    }

    #[test]
    #[ignore = "Requires a GPU; regenerates the foreign-key navigation screenshots without a database"]
    fn render_fk_navigation_screenshots() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/fk-navigation-snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        // A `customer_id` cell (logical points); DATACRAFT_FK_CELL=x,y overrides it. Its value is
        // numeric (right-aligned), so the follow button sits at the cell's left edge.
        let cell = std::env::var("DATACRAFT_FK_CELL")
            .ok()
            .and_then(|v| v.split_once(',').map(|(x, y)| egui::pos2(x.parse().unwrap(), y.parse().unwrap())))
            .unwrap_or(egui::pos2(480.0, 220.0));
        let mut h = harness();
        // Hovering a key value shows its follow button; over the button, the value is a link.
        h.hover_at(cell);
        h.run_steps(2);
        h.hover_at(egui::pos2(418.0, cell.y + 6.0));
        h.run_steps(30);
        h.render().unwrap().save(dir.join("grid-links.png")).unwrap();
        h.hover_at(cell);
        h.event(egui::Event::PointerButton { pos: cell, button: egui::PointerButton::Secondary, pressed: true, modifiers: Modifiers::NONE });
        h.event(egui::Event::PointerButton { pos: cell, button: egui::PointerButton::Secondary, pressed: false, modifiers: Modifiers::NONE });
        h.run_steps(3);
        h.render().unwrap().save(dir.join("context-menu.png")).unwrap();
        h.key_press(Key::Escape);
        h.hover_at(egui::pos2(1000.0, 600.0));
        h.run_steps(2);

        // After "Go to referenced row": the customer, filtered, with history back to the order.
        let app = h.state_mut();
        let origin = app.place(0).unwrap();
        app.fk_nav.back.push(origin);
        app.active = 1;
        app.consoles[1].table.as_mut().unwrap().view.filter_sort = true;
        h.run_steps(2);
        // The history's Back button, with its tooltip.
        h.hover_at(egui::pos2(293.0, 105.0));
        h.run_steps(30);
        h.render().unwrap().save(dir.join("referenced-row.png")).unwrap();

        let app = h.state_mut();
        app.active = 0;
        let origin = app.place(0).unwrap();
        let counts = [Ok(1001), Ok(3), Ok(1), Ok(0)];
        let children = ["audit_events", "order_items", "payments", "shipments"]
            .into_iter()
            .zip(counts)
            .map(|(table, count)| Child { key: key(table, "order_id", "orders"), values: Some(vec![Some("1003".into())]), count: Some(count) })
            .collect();
        app.fk_nav.dialog = Some(Dialog {
            id: 1,
            conn: 7,
            db: "shop".into(),
            driver: Driver::MariaDb,
            subtitle: "shop.orders · id = 1003".into(),
            origin,
            children,
            probed: vec![0, 1, 2, 3],
            cancel: Arc::default(),
            pending: false,
            error: None,
        });
        h.hover_at(egui::pos2(1000.0, 680.0));
        h.run_steps(3);
        h.render().unwrap().save(dir.join("referenced-by.png")).unwrap();
    }
}
