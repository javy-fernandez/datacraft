//! "Compare Schemas…": source and target pickers, the differences grouped by status, each
//! object's definitions side by side and the script that makes the target match the source.

use crate::app::{ConnState, ConnStatus};
use crate::db::schema_compare::read::{Progress, Side};
use crate::db::schema_compare::{self, Comparison, Kind, SchemaModel, Script, Status, changed_lines};
use crate::i18n::{tr, trf};
use crate::sql;
use crate::ui::create_dialog::default_schema;
use crate::ui::icons::{self, Ico};
use crate::ui::theme::{self, Palette, palette};
use crate::worker::ConnId;
use egui::{Align, Color32, FontId, Layout, Margin, RichText, Sense, Stroke, Ui, vec2};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Request ids, unique in the whole app: a late reply to a closed dialog never matches a new one.
static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);

/// One side as picked in the dialog.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pick {
    pub conn: ConnId,
    pub db: String,
    pub schema: String,
}

/// A finished comparison and the objects chosen for the script.
pub struct Outcome {
    /// The pair that was compared: the script, its console and its warnings belong to it, not
    /// to what the pickers show now.
    pub pair: (Pick, Pick),
    pub source: SchemaModel,
    pub target: SchemaModel,
    pub comparison: Comparison,
    pub include: Vec<bool>,
    pub script: Result<Script, String>,
    pub text: String,
    /// Changed lines of each side's definition, by object, computed once when first shown.
    pub lines: std::collections::HashMap<usize, (Vec<bool>, Vec<bool>)>,
}

impl Outcome {
    pub fn new(pair: (Pick, Pick), source: SchemaModel, target: SchemaModel) -> Self {
        let comparison = schema_compare::compare(&source, &target);
        let include = comparison.default_selection();
        let mut o = Self { pair, source, target, comparison, include, script: Ok(Script::default()), text: String::new(), lines: Default::default() };
        o.refresh();
        o
    }

    /// Regenerates the script after the selection changed.
    pub fn refresh(&mut self) {
        self.script = schema_compare::script(&self.source, &self.target, &self.comparison, &self.include);
        self.text = match &self.script {
            Ok(s) => s.text(&self.source, &self.target),
            Err(_) => String::new(),
        };
    }

    fn scriptable(&self) -> bool {
        self.script.as_ref().is_ok_and(|s| !s.statements.is_empty())
    }
}

pub struct CompareDialog {
    pub source: Pick,
    pub target: Pick,
    pub request_id: u64,
    progress: Option<Arc<Progress>>,
    /// The pair being compared.
    pending: Option<(Pick, Pick)>,
    pub outcome: Option<Outcome>,
    pub error: Option<String>,
    /// Object shown on the right (index in the comparison).
    pub selected: Option<usize>,
    pub show_identical: bool,
    collapsed: [bool; 4],
}

pub enum CompareAction {
    Start { source: ConnId, target: ConnId, sides: Box<(Side, Side)>, progress: Arc<Progress> },
    /// A database's schema is needed for the pickers.
    Load(ConnId, String),
    /// The script, with the target it was made for.
    Save { schema: String, text: String },
    OpenConsole { target: Pick, text: String },
    Close,
}

fn usable(c: &ConnState) -> bool {
    c.status == ConnStatus::Connected && schema_compare::supported(c.cfg.driver)
}

/// Status colours: created (source only), dropped (target only), changed, same.
fn status_color(p: &Palette, s: Status) -> Color32 {
    match s {
        Status::OnlyInSource => p.ok,
        Status::OnlyInTarget => p.error,
        Status::Different => p.warn,
        Status::Identical => p.muted,
    }
}

impl CompareDialog {
    /// Opened on `conn` (and its database and schema, when chosen there): the source. The target
    /// is another connection of the same engine if there is one.
    pub fn new(conns: &[ConnState], conn: ConnId, db: Option<String>, schema: Option<String>) -> Self {
        let source_conn = conns.iter().find(|c| c.cfg.id == conn);
        let driver = source_conn.map(|c| c.cfg.driver);
        let db = db.or_else(|| source_conn.and_then(|c| c.default_db().map(str::to_string))).unwrap_or_default();
        let target = conns
            .iter()
            .filter(|c| usable(c) && c.cfg.id != conn)
            .find(|c| Some(c.cfg.driver) == driver)
            .map_or(conn, |c| c.cfg.id);
        let mut d = Self {
            source: Pick { conn, db, schema: schema.unwrap_or_default() },
            target: Pick { conn: target, ..Default::default() },
            request_id: 0,
            progress: None,
            pending: None,
            outcome: None,
            error: None,
            selected: None,
            show_identical: false,
            collapsed: [false, false, false, true],
        };
        d.pick_db(conns, true, None);
        d
    }

    pub fn running(&self) -> bool {
        self.progress.is_some()
    }

    /// Target database: `db`, or the source's name when it has it, or its default one.
    fn pick_db(&mut self, conns: &[ConnState], target: bool, db: Option<String>) {
        let wanted = self.source.db.clone();
        let pick = if target { &mut self.target } else { &mut self.source };
        pick.schema.clear();
        let Some(c) = conns.iter().find(|c| c.cfg.id == pick.conn) else { return };
        pick.db = db.unwrap_or_else(|| {
            if c.databases.iter().any(|d| d.name == wanted) { wanted } else { c.default_db().unwrap_or_default().to_string() }
        });
    }

    /// Fills an empty schema once the database is loaded; asks for it otherwise.
    fn settle(&mut self, conns: &[ConnState], target: bool) -> Option<CompareAction> {
        let other = if target { self.source.schema.clone() } else { String::new() };
        let pick = if target { &mut self.target } else { &mut self.source };
        let c = conns.iter().find(|c| c.cfg.id == pick.conn)?;
        let Some(schemas) = c.schemas(Some(&pick.db)) else {
            return (!pick.db.is_empty() && !c.loading.contains(&pick.db)).then(|| CompareAction::Load(pick.conn, pick.db.clone()));
        };
        if pick.schema.is_empty() || !schemas.iter().any(|s| s.name == pick.schema) {
            let default = default_schema(c.cfg.driver);
            let s = schemas
                .iter()
                .find(|s| !other.is_empty() && s.name == other)
                .or_else(|| schemas.iter().find(|s| s.name == default))
                .or_else(|| schemas.first());
            pick.schema = s.map(|s| s.name.clone()).unwrap_or_default();
        }
        None
    }

    /// The pickers no longer show the pair the result is for.
    pub fn stale(&self) -> bool {
        self.outcome.as_ref().is_some_and(|o| o.pair != (self.source.clone(), self.target.clone()))
    }

    pub(crate) fn start(&mut self) -> CompareAction {
        let progress = Arc::new(Progress::default());
        self.request_id = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
        self.progress = Some(progress.clone());
        self.pending = Some((self.source.clone(), self.target.clone()));
        self.error = None;
        let side = |p: &Pick| Side { db: p.db.clone(), schema: p.schema.clone() };
        CompareAction::Start { source: self.source.conn, target: self.target.conn, sides: Box::new((side(&self.source), side(&self.target))), progress }
    }

    /// Stops the comparison under way (its reply is ignored).
    pub fn cancel(&mut self) {
        if let Some(p) = self.progress.take() {
            p.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Reply for `request_id`; ignored unless it is the comparison under way.
    pub fn finished(&mut self, request_id: u64, result: Result<Box<(SchemaModel, SchemaModel)>, String>) {
        if request_id != self.request_id || self.progress.take().is_none() {
            return;
        }
        let pair = self.pending.take().unwrap_or_default();
        match result {
            Ok(models) => {
                let (source, target) = *models;
                let outcome = Outcome::new(pair, source, target);
                self.selected = outcome.comparison.objects.iter().position(|o| o.status == Status::Different).or((!outcome.comparison.objects.is_empty()).then_some(0));
                self.outcome = Some(outcome);
            }
            Err(e) => self.error = Some(e),
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, conns: &[ConnState]) -> Option<CompareAction> {
        let mut action = self.settle(conns, false).or_else(|| self.settle(conns, true));
        let dark = ctx.global_style().visuals.dark_mode;
        let p = palette(dark);
        let name = |id: ConnId| conns.iter().find(|c| c.cfg.id == id).map(|c| c.cfg.name.clone()).unwrap_or_default();
        // A result speaks of the pair it compared.
        let (source, target) = self.outcome.as_ref().map_or((&self.source, &self.target), |o| (&o.pair.0, &o.pair.1));
        let side = |p: &Pick| if p.schema.is_empty() || p.schema == p.db { format!("{} · {}", name(p.conn), p.db) } else { format!("{} · {}.{}", name(p.conn), p.db, p.schema) };
        let subtitle = format!("{} → {}", side(source), side(target));
        let target_read_only = conns.iter().find(|c| c.cfg.id == target.conn).is_some_and(|c| c.cfg.read_only);
        let stale = self.stale();
        crate::ui::workflow_dialog::show(ctx, egui::Id::new("schema_compare"), tr("Compare Schemas"), &subtitle, vec2(1360.0, 900.0), |root, regions| {
            let mut body = regions.body_ui(root);
            if let Some(a) = self.pickers(&mut body, conns, &p) {
                action = Some(a);
            }
            body.add_space(10.0);
            if let Some(e) = &self.error {
                body.add(egui::Label::new(RichText::new(e).color(p.error)).wrap());
            } else if self.outcome.is_some() {
                self.results(&mut body, &p, dark);
            } else if !self.running() {
                body.add_space(30.0);
                body.vertical_centered(|ui| {
                    ui.label(RichText::new(tr("Choose the source and the target, then compare.")).font(theme::bold(14.0)).color(p.text));
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(tr("The script makes the target match the source. It is never run here: copy it, save it or open it in a console to review it first."))
                            .color(p.muted),
                    );
                });
            }
            let mut footer = regions.footer_ui(root);
            footer.horizontal_centered(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                // Nothing leaves the dialog from a result the pickers no longer match.
                let ready = self.outcome.as_ref().filter(|o| o.scriptable() && !stale).map(|o| (o.text.clone(), o.pair.1.clone()));
                ui.add_enabled_ui(ready.is_some(), |ui| {
                    if theme::secondary_button(ui, tr("Copy")).clicked()
                        && let Some((text, _)) = &ready
                    {
                        ui.ctx().copy_text(text.clone());
                    }
                    if theme::secondary_button(ui, tr("Save .sql…")).clicked()
                        && let Some((text, target)) = &ready
                    {
                        action = Some(CompareAction::Save { schema: target.schema.clone(), text: text.clone() });
                    }
                    if theme::primary_button(ui, tr("Open in Console")).clicked()
                        && let Some((text, target)) = &ready
                    {
                        action = Some(CompareAction::OpenConsole { target: target.clone(), text: text.clone() });
                    }
                });
                if stale {
                    ui.label(RichText::new("⚠").color(p.warn));
                    ui.label(RichText::new(tr("The source or target changed: compare again to update the result.")).small().color(p.warn));
                } else if target_read_only {
                    ui.label(RichText::new("⚠").color(p.warn));
                    ui.label(RichText::new(tr("The target connection is read-only: the script can be reviewed, not run.")).small().color(p.warn));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if theme::secondary_button(ui, tr("Close")).clicked() {
                        action = Some(CompareAction::Close);
                    }
                });
            });
        });
        if action.is_none() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            action = Some(CompareAction::Close);
        }
        action
    }

    /// Source → target pickers and the Compare button (Cancel and progress while it runs).
    fn pickers(&mut self, ui: &mut Ui, conns: &[ConnState], p: &Palette) -> Option<CompareAction> {
        let mut action = None;
        let running = self.running();
        ui.horizontal_top(|ui| {
            ui.add_enabled_ui(!running, |ui| {
                for target in [false, true] {
                    if target {
                        ui.add_space(6.0);
                        ui.vertical(|ui| {
                            ui.add_space(46.0);
                            ui.label(RichText::new("→").font(theme::bold(20.0)).color(p.muted));
                        });
                        ui.add_space(6.0);
                    }
                    // A vertical parent: the frame's heading goes above its grid.
                    ui.vertical(|ui| self.side(ui, conns, p, target));
                }
            });
            ui.add_space(16.0);
            ui.vertical(|ui| {
                ui.add_space(26.0);
                let ready = !running && [&self.source, &self.target].iter().all(|s| !s.db.is_empty() && !s.schema.is_empty());
                if running {
                    if theme::secondary_button(ui, tr("Cancel")).clicked() {
                        self.cancel();
                    }
                    if let Some(progress) = &self.progress {
                        let (done, total) = (progress.done.load(Ordering::Relaxed), progress.total.load(Ordering::Relaxed));
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.add(egui::Spinner::new().size(14.0).color(p.muted));
                            let text = if total == 0 { tr("Reading the catalogs…").to_string() } else { trf("Reading {done} of {total} objects…", &[("done", &done), ("total", &total)]) };
                            ui.label(RichText::new(text).color(p.muted));
                        });
                    }
                } else {
                    let label = if self.outcome.is_some() { tr("Compare Again") } else { tr("Compare") };
                    if ui.add_enabled_ui(ready, |ui| theme::primary_button(ui, label)).inner.clicked() {
                        action = Some(self.start());
                    }
                }
            });
        });
        action
    }

    /// Connection, database and schema of one side.
    fn side(&mut self, ui: &mut Ui, conns: &[ConnState], p: &Palette, target: bool) {
        let mut new_conn = None;
        let mut new_db = None;
        egui::Frame::NONE.stroke(Stroke::new(1.0, p.control_border)).corner_radius(8).inner_margin(Margin::same(12)).show(ui, |ui| {
            ui.set_width(430.0);
            ui.label(RichText::new(if target { tr("Target") } else { tr("Source") }).font(theme::bold(13.0)).color(p.text));
            ui.add_space(6.0);
            let pick = if target { &mut self.target } else { &mut self.source };
            egui::Grid::new(("compare_side", target)).num_columns(2).spacing([12.0, 8.0]).min_col_width(90.0).show(ui, |ui| {
                ui.label(tr("Connection:"));
                let current = conns.iter().find(|c| c.cfg.id == pick.conn);
                egui::ComboBox::from_id_salt(("compare_conn", target)).selected_text(current.map(|c| c.cfg.name.clone()).unwrap_or_default()).width(300.0).show_ui(ui, |ui| {
                    for c in conns.iter().filter(|c| usable(c)) {
                        if ui.selectable_label(c.cfg.id == pick.conn, format!("{} · {}", c.cfg.name, c.cfg.driver.label())).clicked() && c.cfg.id != pick.conn {
                            new_conn = Some(c.cfg.id);
                        }
                    }
                });
                ui.end_row();
                let Some(c) = current else { return };
                ui.label(tr("Database:"));
                egui::ComboBox::from_id_salt(("compare_db", target)).selected_text(&pick.db).width(300.0).show_ui(ui, |ui| {
                    for d in &c.databases {
                        if ui.selectable_label(d.name == pick.db, &d.name).clicked() && d.name != pick.db {
                            new_db = Some(d.name.clone());
                        }
                    }
                });
                ui.end_row();
                ui.label(tr("Schema:"));
                match c.schemas(Some(&pick.db)) {
                    Some(schemas) if !c.cfg.driver.flat_schema() => {
                        egui::ComboBox::from_id_salt(("compare_schema", target)).selected_text(&pick.schema).width(300.0).show_ui(ui, |ui| {
                            for s in schemas {
                                ui.selectable_value(&mut pick.schema, s.name.clone(), &s.name);
                            }
                        });
                    }
                    Some(_) => {
                        ui.label(RichText::new(&pick.schema).color(p.muted));
                    }
                    None => {
                        ui.label(RichText::new(tr("loading…")).color(p.muted));
                    }
                }
                ui.end_row();
            });
        });
        if let Some(id) = new_conn {
            if target {
                self.target.conn = id;
            } else {
                self.source.conn = id;
            }
            self.pick_db(conns, target, None);
        }
        if let Some(db) = new_db {
            self.pick_db(conns, target, Some(db));
        }
    }

    fn results(&mut self, ui: &mut Ui, p: &Palette, dark: bool) {
        let Some(o) = self.outcome.as_mut() else { return };
        let c = &o.comparison;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 14.0;
            for s in Status::ALL {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 5.0;
                    ui.label(RichText::new("●").color(status_color(p, s)));
                    ui.label(RichText::new(s.label()).color(p.text));
                    ui.label(RichText::new(c.count(s).to_string()).strong().color(p.text));
                });
            }
            ui.checkbox(&mut self.show_identical, tr("Show identical"));
        });
        if let Err(e) = &o.script {
            ui.add(egui::Label::new(RichText::new(e).color(p.warn)).wrap());
        }
        // What wasn't compared is always shown: "no differences" never covers it.
        for (objects, label) in [(&c.skipped_source, tr("Not compared in the source: {objects}")), (&c.skipped_target, tr("Not compared in the target: {objects}"))] {
            if !objects.is_empty() {
                let text = label.replace("{objects}", &objects.join(", "));
                ui.add(egui::Label::new(RichText::new(format!("⚠ {text}")).small().color(p.warn)).wrap());
            }
        }
        ui.add_space(8.0);
        if !c.has_differences() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                let (title, color) = if c.complete() { (tr("No differences"), p.ok) } else { (tr("No differences in the compared objects"), p.warn) };
                ui.label(RichText::new(title).font(theme::bold(16.0)).color(color));
                ui.label(RichText::new(trf("{n} objects are identical.", &[("n", &c.objects.len())])).color(p.muted));
            });
            return;
        }
        let height = ui.available_height();
        let width = ui.available_width();
        let list_w = 320.0_f32.min(width * 0.35);
        let mut changed = false;
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(vec2(list_w, height), Layout::top_down(Align::Min), |ui| {
                ui.set_min_size(vec2(list_w, height));
                changed = object_list(ui, o, &mut self.selected, &mut self.collapsed, self.show_identical, p);
            });
            ui.add_space(4.0);
            let rest = ui.available_width();
            ui.allocate_ui_with_layout(vec2(rest, height), Layout::top_down(Align::Min), |ui| {
                ui.set_min_size(vec2(rest, height));
                if let Some(obj) = self.selected.and_then(|i| o.comparison.objects.get(i)) {
                    let i = self.selected.unwrap_or_default();
                    o.lines.entry(i).or_insert_with(|| changed_lines(&obj.source_ddl, &obj.target_ddl));
                }
                detail(ui, o, self.selected, p, dark, height);
            });
        });
        if changed {
            o.refresh();
        }
    }
}

/// The objects grouped by status, with their checkboxes; true when the selection changed.
fn object_list(ui: &mut Ui, o: &mut Outcome, selected: &mut Option<usize>, collapsed: &mut [bool; 4], show_identical: bool, p: &Palette) -> bool {
    let mut changed = false;
    let scriptable = o.script.is_ok();
    egui::Frame::NONE.stroke(Stroke::new(1.0, p.control_border)).corner_radius(6).inner_margin(Margin::same(6)).show(ui, |ui| {
        egui::ScrollArea::vertical().id_salt("compare_objects").auto_shrink([false, false]).show(ui, |ui| {
            for (g, status) in Status::ALL.into_iter().enumerate() {
                let n = o.comparison.count(status);
                if n == 0 || (status == Status::Identical && !show_identical) {
                    continue;
                }
                let header = ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
                    icons::paint(ui.painter(), rect, if collapsed[g] { Ico::ChevronRight } else { Ico::ChevronDown }, p.muted, p, true);
                    ui.add(egui::Label::new(RichText::new(format!("{} ({n})", status.label())).font(theme::bold(12.5)).color(status_color(p, status))).sense(Sense::click()))
                });
                if header.inner.clicked() || header.response.clicked() {
                    collapsed[g] = !collapsed[g];
                }
                if collapsed[g] {
                    continue;
                }
                for (i, obj) in o.comparison.objects.iter().enumerate().filter(|(_, x)| x.status == status) {
                    ui.horizontal(|ui| {
                        ui.add_space(14.0);
                        // Identical objects have nothing to script: an invisible box keeps the column.
                        if status == Status::Identical {
                            ui.add_visible(false, egui::Checkbox::without_text(&mut false));
                        } else if ui.add_enabled(scriptable && obj.manual.is_none(), egui::Checkbox::without_text(&mut o.include[i])).changed() {
                            changed = true;
                        }
                        let (rect, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
                        icons::paint(ui.painter(), rect, if obj.kind == Kind::View { Ico::View } else { Ico::Table }, p.icon, p, true);
                        let color = if status == Status::OnlyInTarget { p.error } else { p.text };
                        if ui.selectable_label(*selected == Some(i), RichText::new(&obj.name).color(color)).clicked() {
                            *selected = Some(i);
                        }
                        let (tag, color) = match status {
                            _ if obj.manual.is_some() => (tr("manual"), p.error),
                            Status::Different if obj.destructive => (tr("alter, loses data"), p.error),
                            Status::OnlyInSource => (tr("create"), status_color(p, status)),
                            Status::OnlyInTarget => (tr("drop"), status_color(p, status)),
                            Status::Different => (tr("alter"), status_color(p, status)),
                            Status::Identical => ("", status_color(p, status)),
                        };
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(RichText::new(tag).small().color(color));
                        });
                    });
                }
                ui.add_space(4.0);
            }
        });
    });
    changed
}

/// The selected object's changes and definitions side by side, then the script.
fn detail(ui: &mut Ui, o: &Outcome, selected: Option<usize>, p: &Palette, dark: bool, height: f32) {
    let top = (height * 0.56).max(160.0);
    if let Some(obj) = selected.and_then(|i| o.comparison.objects.get(i)) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(&obj.name).font(theme::bold(14.0)).color(p.text));
            ui.label(RichText::new(obj.status.label()).color(status_color(p, obj.status)));
            if obj.status == Status::OnlyInTarget {
                ui.label(RichText::new(tr("Dropping it deletes its data: it is left out of the script unless checked.")).small().color(p.error));
            } else if obj.destructive {
                ui.label(RichText::new(tr("Drops columns or changes their type: left out of the script unless checked.")).small().color(p.error));
            }
        });
        if let Some(why) = &obj.manual {
            ui.add(egui::Label::new(RichText::new(trf("Needs a manual migration, not scripted: {reason}", &[("reason", why)])).small().color(p.error)).wrap());
        }
        if !obj.changes.is_empty() {
            ui.label(RichText::new(obj.changes.join(" · ")).small().color(p.muted));
        }
        ui.add_space(4.0);
        let none = (vec![], vec![]);
        let (a, b) = selected.and_then(|i| o.lines.get(&i)).unwrap_or(&none);
        let pane_w = (ui.available_width() - 8.0) / 2.0;
        let pane_h = top - 50.0;
        let label = |m: &SchemaModel| if m.driver.flat_schema() { m.db.clone() } else { format!("{}.{}", m.db, m.schema) };
        ui.horizontal_top(|ui| {
            definition(ui, "compare_source_ddl", &trf("Source · {name}", &[("name", &label(&o.source))]), &obj.source_ddl, a, p.ok, p, dark, vec2(pane_w, pane_h));
            definition(ui, "compare_target_ddl", &trf("Target · {name}", &[("name", &label(&o.target))]), &obj.target_ddl, b, p.error, p, dark, vec2(pane_w, pane_h));
        });
    } else {
        ui.add_space(top);
    }
    ui.add_space(8.0);
    ui.label(RichText::new(tr("Migration script")).font(theme::bold(13.0)).color(p.text));
    ui.add_space(4.0);
    let rest = ui.available_height().max(80.0);
    egui::Frame::NONE.fill(p.editor_bg).stroke(Stroke::new(1.0, p.control_border)).corner_radius(6).inner_margin(Margin::same(8)).show(ui, |ui| {
        ui.set_min_size(vec2(ui.available_width(), rest - 18.0));
        match &o.script {
            Ok(_) => {
                egui::ScrollArea::both().id_salt("compare_script").auto_shrink([false, false]).max_height(rest - 18.0).show(ui, |ui| {
                    ui.add(egui::Label::new(sql::highlight_for(&o.text, o.target.driver, dark, FontId::monospace(12.0))).wrap_mode(egui::TextWrapMode::Extend));
                });
            }
            Err(e) => {
                ui.add(egui::Label::new(RichText::new(e).color(p.muted)).wrap());
            }
        }
    });
}

/// One side's definition, its changed lines tinted with `tint`.
#[allow(clippy::too_many_arguments)]
fn definition(ui: &mut Ui, id: &str, title: &str, text: &str, changed: &[bool], tint: Color32, p: &Palette, dark: bool, size: egui::Vec2) {
    ui.vertical(|ui| {
        ui.set_width(size.x);
        ui.label(RichText::new(title).small().color(p.muted));
        egui::Frame::NONE.fill(p.editor_bg).stroke(Stroke::new(1.0, p.control_border)).corner_radius(6).inner_margin(Margin::same(4)).show(ui, |ui| {
            ui.set_min_size(vec2(size.x - 10.0, size.y - 8.0));
            egui::ScrollArea::both().id_salt(id).auto_shrink([false, false]).max_height(size.y - 8.0).show(ui, |ui| {
                if text.is_empty() {
                    ui.label(RichText::new(tr("(not present)")).italics().color(p.muted));
                    return;
                }
                ui.spacing_mut().item_spacing.y = 0.0;
                let fill = Color32::from_rgba_unmultiplied(tint.r(), tint.g(), tint.b(), if dark { 46 } else { 34 });
                let width = ui.available_width();
                for (line, on) in text.lines().zip(changed.iter().chain(std::iter::repeat(&false))) {
                    let frame = egui::Frame::NONE.fill(if *on { fill } else { Color32::TRANSPARENT }).inner_margin(Margin::symmetric(6, 1));
                    let r = frame.show(ui, |ui| {
                        ui.set_min_width(width - 12.0);
                        let job = sql::highlight(if line.is_empty() { " " } else { line }, dark, FontId::monospace(12.0));
                        ui.add(egui::Label::new(job).wrap_mode(egui::TextWrapMode::Extend));
                    });
                    if *on {
                        let rect = r.response.rect;
                        ui.painter().vline(rect.left() + 1.0, rect.y_range(), Stroke::new(3.0, tint));
                    }
                }
            });
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{ConnectionConfig, DatabaseInfo, Driver, SchemaInfo};
    use egui_kittest::{Harness, kittest::Queryable};

    fn connections() -> Vec<ConnState> {
        let conn = |id: ConnId, name: &str, schemas: &[&str]| {
            let mut cfg = ConnectionConfig::new(id, Driver::Postgres);
            cfg.name = name.into();
            cfg.database = "shop".into();
            let mut c = ConnState::new(cfg);
            c.status = ConnStatus::Connected;
            let schemas = schemas.iter().map(|s| SchemaInfo { name: s.to_string(), tables: vec![] }).collect();
            c.databases = vec![DatabaseInfo { name: "shop".into(), current: true, schemas: Some(schemas) }];
            c
        };
        vec![conn(1, "Production", &["public", "shop"]), conn(2, "Staging", &["public", "shop", "shop_staging"])]
    }

    /// The dialog on `shop` (production) against `shop_staging`, compared when `result`.
    fn dialog(result: bool) -> CompareDialog {
        let conns = connections();
        let mut d = CompareDialog::new(&conns, 1, Some("shop".into()), Some("shop".into()));
        d.target.schema = "shop_staging".into();
        if result {
            let (src, tgt) = schema_compare::demo_models();
            let outcome = Outcome::new((d.source.clone(), d.target.clone()), src, tgt);
            d.selected = outcome.comparison.objects.iter().position(|o| o.name == "orders");
            d.outcome = Some(outcome);
            d.show_identical = true;
            d.collapsed = [false; 4];
        }
        d
    }

    fn harness(d: CompareDialog, dark: bool) -> Harness<'static, CompareDialog> {
        let conns = connections();
        let mut first = true;
        let mut h = Harness::builder().with_size(vec2(1600.0, 1000.0)).build_ui_state(
            move |ui, d: &mut CompareDialog| {
                if std::mem::take(&mut first) {
                    theme::install_fonts(ui.ctx());
                    theme::apply(ui.ctx(), dark);
                    return;
                }
                let _ = d.show(ui.ctx(), &conns);
            },
            d,
        );
        h.run_steps(4);
        h
    }

    #[test]
    fn a_stale_result_can_not_leave_the_dialog_and_the_console_gets_the_compared_target() {
        struct State {
            d: CompareDialog,
            opened: Option<Pick>,
        }
        let conns = connections();
        let mut first = true;
        let mut h = Harness::builder().with_size(vec2(1600.0, 1000.0)).build_ui_state(
            move |ui, s: &mut State| {
                if std::mem::take(&mut first) {
                    theme::install_fonts(ui.ctx());
                    return;
                }
                if let Some(CompareAction::OpenConsole { target, .. }) = s.d.show(ui.ctx(), &conns) {
                    s.opened = Some(target);
                }
            },
            State { d: dialog(true), opened: None },
        );
        h.run_steps(2);
        assert!(h.query_by_label("Production · shop → Staging · shop.shop_staging").is_some());
        // The target picker moves away from the compared pair: the result is stale.
        h.state_mut().d.target.schema = "public".into();
        h.run_steps(2);
        assert!(h.query_by_label("The source or target changed: compare again to update the result.").is_some());
        assert!(h.query_by_label("Production · shop → Staging · shop.shop_staging").is_some(), "the subtitle names the compared pair");
        h.get_by_label("Open in Console").click();
        h.run_steps(2);
        assert!(h.state().opened.is_none(), "a stale script is not opened");
        h.state_mut().d.target.schema = "shop_staging".into();
        h.run_steps(2);
        h.get_by_label("Open in Console").click();
        h.run_steps(2);
        let opened = h.state().opened.clone().unwrap();
        assert_eq!((opened.conn, opened.db.as_str(), opened.schema.as_str()), (2, "shop", "shop_staging"));
    }

    #[test]
    fn a_manual_migration_is_marked_and_can_not_be_checked() {
        let mut d = dialog(true);
        let o = d.outcome.as_mut().unwrap();
        let i = o.comparison.objects.iter().position(|x| x.name == "orders").unwrap();
        o.comparison.objects[i].manual = Some("rebuilding the table would drop its triggers (audit)".into());
        o.include[i] = false;
        d.selected = Some(i);
        let mut h = harness(d, true);
        assert!(h.query_by_label("manual").is_some());
        assert!(h.query_by_label("Needs a manual migration, not scripted: rebuilding the table would drop its triggers (audit)").is_some());
        // Check boxes: "Show identical", then payments, coupons, orders. Coupons' box works; the
        // manual one stays off.
        let coupons = h.state().outcome.as_ref().unwrap().comparison.objects.iter().position(|x| x.name == "coupons").unwrap();
        h.query_all_by_role(egui::accesskit::Role::CheckBox).nth(2).unwrap().click();
        h.run_steps(2);
        assert!(h.state().outcome.as_ref().unwrap().include[coupons], "the row order is as expected");
        h.query_all_by_role(egui::accesskit::Role::CheckBox).nth(3).unwrap().click();
        h.run_steps(2);
        assert!(!h.state().outcome.as_ref().unwrap().include[i]);
    }

    #[test]
    fn changed_lines_are_computed_once_per_shown_object() {
        let mut h = harness(dialog(true), true);
        let selected = h.state().selected.unwrap();
        let o = h.state().outcome.as_ref().unwrap();
        assert_eq!(o.lines.keys().copied().collect::<Vec<_>>(), [selected]);
        // Later frames reuse it: a marker left in the cache stays.
        h.state_mut().outcome.as_mut().unwrap().lines.insert(selected, (vec![true], vec![]));
        h.run_steps(3);
        assert_eq!(h.state().outcome.as_ref().unwrap().lines[&selected], (vec![true], vec![]));
    }

    #[test]
    fn dialog_shows_the_groups_and_the_target_by_default() {
        let conns = connections();
        let d = CompareDialog::new(&conns, 1, Some("shop".into()), None);
        assert_eq!((d.target.conn, d.target.db.as_str()), (2, "shop"), "another connection of the same engine");
        let h = harness(dialog(true), true);
        for label in ["Compare Schemas", "Only in source (1)", "Only in target (1)", "Different (2)", "Identical (4)", "Migration script", "Open in Console"] {
            assert!(h.query_by_label(label).is_some(), "{label}");
        }
        let h = harness(dialog(false), false);
        assert!(h.query_by_label("Choose the source and the target, then compare.").is_some());
    }

    #[test]
    #[ignore = "Requires a GPU; regenerates the Compare Schemas screenshots without a database"]
    fn render_schema_compare() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/schema-compare-snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        for (result, dark, file) in [(true, true, "compare-result-dark.png"), (false, true, "compare-pickers-dark.png"), (true, false, "compare-result-light.png")] {
            let mut h = harness(dialog(result), dark);
            h.render().unwrap().save(dir.join(file)).unwrap();
        }
    }
}
