//! "Copy Table To…" (DataGrip): copies a table's rows to another connection (or another
//! database or schema of the same one), creating the target table when it doesn't exist.

use crate::app::{ConnState, ConnStatus};
use crate::db::copy::{self, CopyRequest, Mask};
use crate::db::{ColumnInfo, Driver};
use crate::i18n::{tr, trf};
use crate::sql;
use crate::ui::create_dialog::default_schema;
use crate::ui::theme::{self, palette};
use crate::worker::ConnId;
use egui::{FontId, Margin, RichText, vec2};
use std::sync::Arc;
use tokio::sync::Notify;

pub struct CopyDialog {
    pub source: ConnId,
    source_driver: Driver,
    pub db: String,
    pub schema: String,
    pub table: String,
    columns: Vec<ColumnInfo>,
    /// Source columns to copy (binary ones can't be).
    selected: Vec<bool>,
    /// Masking rule of each source column.
    masks: Vec<Mask>,
    pub target: ConnId,
    pub target_db: String,
    pub target_schema: String,
    pub target_table: String,
    filter: String,
    percent: u8,
    clear: bool,
    /// Subset: also the rows they reference / the rows that reference them.
    parents: bool,
    children: bool,
    mask_related: bool,
    pub request_id: u64,
    cancel: Option<Arc<Notify>>,
    pub error: Option<String>,
}

pub enum CopyAction {
    /// Validate (read-only filter) and send.
    Start(Box<CopyRequest>),
    /// The target database's schema is needed.
    Load(ConnId, String),
    Close,
}

/// What the dialog knows of the target table: not loaded yet, missing, or its columns.
enum Target<'a> {
    Loading,
    New,
    Existing(&'a [ColumnInfo]),
}

impl CopyDialog {
    pub fn new(
        source: &ConnState,
        db: String,
        schema: String,
        table: String,
        columns: Vec<ColumnInfo>,
        conns: &[ConnState],
    ) -> Self {
        let driver = source.cfg.driver;
        let selected = columns
            .iter()
            .map(|c| !copy::is_binary(driver, &c.data_type))
            .collect();
        let n_columns = columns.len();
        // prod → dev: another writable connection first, the same one otherwise.
        let target = conns
            .iter()
            .filter(|c| writable(c))
            .find(|c| c.cfg.id != source.cfg.id)
            .or_else(|| conns.iter().find(|c| writable(c)))
            .map_or(source.cfg.id, |c| c.cfg.id);
        let mut d = Self {
            source: source.cfg.id,
            source_driver: driver,
            db,
            schema,
            target_table: table.clone(),
            table,
            columns,
            selected,
            masks: vec![Mask::Keep; n_columns],
            target,
            target_db: String::new(),
            target_schema: String::new(),
            filter: String::new(),
            percent: 100,
            clear: false,
            parents: false,
            children: false,
            mask_related: true,
            request_id: 0,
            cancel: None,
            error: None,
        };
        d.pick_target(conns, target);
        d
    }

    pub fn running(&self) -> bool {
        self.cancel.is_some()
    }

    /// Selects a target connection and its default database.
    fn pick_target(&mut self, conns: &[ConnState], id: ConnId) {
        self.target = id;
        let Some(c) = conns.iter().find(|c| c.cfg.id == id) else {
            return;
        };
        let same_db = c.databases.iter().any(|d| d.name == self.db);
        self.target_db = if same_db {
            self.db.clone()
        } else {
            c.default_db().unwrap_or_default().to_string()
        };
        self.target_schema.clear();
    }

    /// Fills the target schema once its database is loaded: the source's schema name, the
    /// engine's default, or the first one.
    fn settle_schema(&mut self, conns: &[ConnState]) {
        if !self.target_schema.is_empty() {
            return;
        }
        let Some(c) = conns.iter().find(|c| c.cfg.id == self.target) else {
            return;
        };
        let Some(schemas) = c.schemas(Some(&self.target_db)) else {
            return;
        };
        let default = default_schema(c.cfg.driver);
        let pick = schemas
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(&self.schema))
            .or_else(|| schemas.iter().find(|s| s.name == default))
            .or_else(|| schemas.first());
        if let Some(s) = pick {
            self.target_schema.clone_from(&s.name);
        }
    }

    fn target_state<'a>(&self, conns: &'a [ConnState]) -> Target<'a> {
        let Some(c) = conns.iter().find(|c| c.cfg.id == self.target) else {
            return Target::Loading;
        };
        let Some(schemas) = c.schemas(Some(&self.target_db)) else {
            return Target::Loading;
        };
        let table = schemas
            .iter()
            .find(|s| s.name == self.target_schema)
            .and_then(|s| {
                s.tables
                    .iter()
                    .find(|t| t.name.eq_ignore_ascii_case(self.target_table.trim()))
            });
        match table {
            Some(t) => Target::Existing(&t.columns),
            None => Target::New,
        }
    }

    /// Source columns to copy and the target name of each: on an existing table, only those it
    /// has (matched ignoring case).
    /// A MongoDB collection takes any field: an existing one keeps the spelling of the fields
    /// its sample found.
    fn plan(&self, target: &Target, to: Driver) -> (Vec<ColumnInfo>, Vec<String>, Vec<Mask>) {
        let mut plan = (Vec::new(), Vec::new(), Vec::new());
        for ((c, on), mask) in self.columns.iter().zip(&self.selected).zip(&self.masks) {
            let name = match target {
                _ if !on => None,
                Target::Existing(cols) => {
                    match cols.iter().find(|t| t.name.eq_ignore_ascii_case(&c.name)) {
                        Some(t) => Some(t.name.clone()),
                        None if to == Driver::MongoDb => Some(c.name.clone()),
                        None => None,
                    }
                }
                _ => Some(c.name.clone()),
            };
            if let Some(name) = name {
                plan.0.push(c.clone());
                plan.1.push(name);
                plan.2.push(*mask);
            }
        }
        plan
    }

    /// "Suggest masking": a rule for each column whose name looks personal (email, phone…).
    pub fn suggest_masks(&mut self) {
        for (c, mask) in self.columns.iter().zip(&mut self.masks) {
            let s = Mask::suggest(&c.name);
            *mask = if s.allowed(self.source_driver, c) {
                s
            } else {
                Mask::Keep
            };
        }
    }

    fn target_driver(&self, conns: &[ConnState]) -> Option<Driver> {
        conns
            .iter()
            .find(|c| c.cfg.id == self.target)
            .map(|c| c.cfg.driver)
    }

    /// The copy was sent: remember how to cancel it.
    pub fn started(&mut self, cancel: Arc<Notify>) {
        self.request_id = self.request_id.wrapping_add(1);
        self.cancel = Some(cancel);
        self.error = None;
    }

    /// Reply for `request_id`; true if it is this dialog's current copy.
    pub fn finished(&mut self, request_id: u64) -> bool {
        if request_id != self.request_id || self.cancel.is_none() {
            return false;
        }
        self.cancel = None;
        true
    }

    fn stop(&mut self) {
        if let Some(c) = &self.cancel {
            c.notify_one();
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, conns: &[ConnState]) -> Option<CopyAction> {
        let mut result = None;
        self.settle_schema(conns);
        let target = self.target_state(conns);
        if matches!(target, Target::Loading) && !self.target_db.is_empty() {
            result = Some(CopyAction::Load(self.target, self.target_db.clone()));
        }
        let to = self.target_driver(conns).unwrap_or(self.source_driver);
        let (columns, names, masks) = self.plan(&target, to);
        // MongoDB on either side: a JSON filter, no foreign keys to follow, no transaction.
        let mongo_source = self.source_driver == Driver::MongoDb;
        let mongo = mongo_source || to == Driver::MongoDb;
        let creating = matches!(target, Target::New);
        let running = self.running();
        let dark = ctx.global_style().visuals.dark_mode;
        let p = palette(dark);
        let frame = egui::Frame::popup(&ctx.global_style())
            .fill(p.popup_bg)
            .corner_radius(12)
            .inner_margin(Margin::same(20))
            .stroke(egui::Stroke::new(1.0, p.popup_border));
        let source_name = conns
            .iter()
            .find(|c| c.cfg.id == self.source)
            .map(|c| c.cfg.name.clone())
            .unwrap_or_default();
        let mut new_target = None;
        egui::Modal::new(egui::Id::new("copy_dialog")).frame(frame).show(ctx, |ui| {
            ui.set_width(640.0_f32.min(ctx.content_rect().width() - 60.0).max(300.0));
            ui.label(RichText::new(tr("Copy Table")).font(theme::bold(15.0)).color(p.text));
            let from = if self.source_driver.flat_schema() { self.table.clone() } else { format!("{}.{}", self.schema, self.table) };
            ui.label(RichText::new(trf("from {table} in {conn} · {db}", &[("table", &from), ("conn", &source_name), ("db", &self.db)])).color(p.muted));
            ui.add_space(10.0);
            ui.add_enabled_ui(!running, |ui| {
                egui::Grid::new("copy_options").num_columns(2).spacing([14.0, 10.0]).min_col_width(90.0).show(ui, |ui| {
                    ui.label(tr("Connection:"));
                    let current = conns.iter().find(|c| c.cfg.id == self.target).map(|c| c.cfg.name.clone()).unwrap_or_default();
                    egui::ComboBox::from_id_salt("copy_target").selected_text(current).width(260.0).show_ui(ui, |ui| {
                        for c in conns.iter().filter(|c| writable(c)) {
                            if ui.selectable_label(c.cfg.id == self.target, &c.cfg.name).clicked() {
                                new_target = Some(c.cfg.id);
                            }
                        }
                    });
                    ui.end_row();
                    if let Some(c) = conns.iter().find(|c| c.cfg.id == self.target) {
                        if c.databases.len() > 1 {
                            ui.label(tr("Database:"));
                            egui::ComboBox::from_id_salt("copy_db").selected_text(&self.target_db).width(260.0).show_ui(ui, |ui| {
                                for db in &c.databases {
                                    if ui.selectable_label(db.name == self.target_db, &db.name).clicked() && db.name != self.target_db {
                                        self.target_db.clone_from(&db.name);
                                        self.target_schema.clear();
                                    }
                                }
                            });
                            ui.end_row();
                        }
                        let schemas = c.schemas(Some(&self.target_db)).unwrap_or_default();
                        if !c.cfg.driver.flat_schema() && !schemas.is_empty() {
                            ui.label(tr("Schema:"));
                            egui::ComboBox::from_id_salt("copy_schema").selected_text(&self.target_schema).width(260.0).show_ui(ui, |ui| {
                                for s in schemas {
                                    ui.selectable_value(&mut self.target_schema, s.name.clone(), &s.name);
                                }
                            });
                            ui.end_row();
                        }
                    }
                    ui.label(if to == Driver::MongoDb { tr("Collection:") } else { tr("Table:") });
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.target_table).margin(vec2(8.0, 5.0)).desired_width(260.0));
                        let note = match target {
                            Target::Loading => tr("loading…"),
                            Target::New => tr("will be created"),
                            Target::Existing(_) => tr("exists: rows are added"),
                        };
                        ui.label(RichText::new(note).small().color(p.muted));
                    });
                    ui.end_row();
                    ui.label(if mongo_source { tr("Filter:") } else { tr("WHERE:") });
                    let hint = if mongo_source { tr("optional, e.g. {status: \"active\"}") } else { tr("optional, e.g. created_at > '2026-01-01'") };
                    ui.add(
                        egui::TextEdit::singleline(&mut self.filter)
                            .margin(vec2(8.0, 5.0))
                            .desired_width(400.0)
                            .font(egui::TextStyle::Monospace)
                            .hint_text(hint),
                    );
                    ui.end_row();
                    ui.label(tr("Rows:"));
                    ui.horizontal(|ui| {
                        // The theme hides a slider's track: a drag field reads better.
                        ui.add(egui::DragValue::new(&mut self.percent).range(1..=100).speed(0.5).suffix(" %"));
                        if self.percent == 100 {
                            ui.label(RichText::new(tr("all")).small().color(p.muted));
                        }
                    });
                    ui.end_row();
                    if matches!(target, Target::Existing(_)) {
                        ui.label("");
                        let label = if to == Driver::MongoDb { tr("Delete the collection's documents first") } else { tr("Delete the target table's rows first") };
                        ui.checkbox(&mut self.clear, label);
                        ui.end_row();
                    }
                    if mongo {
                        return;
                    }
                    ui.label(tr("Related rows:"));
                    ui.vertical(|ui| {
                        // The rows that reference them need the ones they reference too.
                        ui.add_enabled_ui(!self.children, |ui| {
                            let mut parents = self.parents || self.children;
                            if ui.checkbox(&mut parents, tr("Rows they reference (needed by the foreign keys)")).changed() {
                                self.parents = parents;
                            }
                        });
                        ui.checkbox(&mut self.children, tr("Rows that reference them (e.g. their orders and order lines)"));
                        if self.parents || self.children {
                            ui.checkbox(&mut self.mask_related, tr("Mask related tables by column name (email, phone, name…)"));
                            ui.label(
                                RichText::new(tr("Related tables go to the same target schema with their own names; rows already there (same primary key) are skipped."))
                                    .small()
                                    .color(p.muted),
                            );
                        }
                    });
                    ui.end_row();
                });
                ui.add_space(10.0);
                self.columns_ui(ui, &target, to, &p);
            });
            ui.add_space(10.0);
            let mut preview = if mongo_source {
                copy::find_command(&self.table, &columns, &self.filter, None)
            } else {
                copy::select_sql(self.source_driver, &self.schema, &self.table, &columns, &self.filter, None)
            };
            if to == Driver::MongoDb && !self.target_table.trim().is_empty() {
                let types: Vec<String> = columns.iter().map(|c| copy::target_type(self.source_driver, to, &c.data_type, c.primary_key)).collect();
                let types: Vec<&str> = types.iter().map(String::as_str).collect();
                let clear = self.clear && !creating;
                preview.push_str(";\n\n");
                preview.push_str(&copy::mongo_insert_preview(self.target_table.trim(), &names, &types, clear));
            } else if creating && !self.target_table.trim().is_empty() {
                let mut cols = columns.clone();
                for (c, n) in cols.iter_mut().zip(&names) {
                    c.name.clone_from(n);
                }
                preview.push_str(";\n\n");
                preview.push_str(&copy::create_sql(self.source_driver, to, &self.target_schema, self.target_table.trim(), &cols));
            }
            egui::Frame::NONE
                .fill(p.editor_bg)
                .stroke(egui::Stroke::new(1.0, p.control_border))
                .corner_radius(6)
                .inner_margin(Margin::same(8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    egui::ScrollArea::vertical().id_salt("copy_sql").max_height(150.0).show(ui, |ui| {
                        let mut job = sql::highlight(&preview, dark, FontId::monospace(12.0));
                        job.wrap.max_width = ui.available_width();
                        ui.label(job);
                    });
                });
            ui.add_space(6.0);
            let note = if to == Driver::MongoDb {
                tr("Up to 1,000,000 documents, inserted in batches of 500 without a transaction (a standalone MongoDB server has none): an error or a stop keeps those already inserted. Documents whose _id is already in the collection are skipped. Binary fields are not copied.")
            } else if to == Driver::ClickHouse {
                tr("Up to 1,000,000 rows, inserted in batches of 100 without a transaction (ClickHouse has none): an error or a stop keeps the batches already inserted, and deleting the target's rows first can't be undone.")
            } else {
                tr("Up to 1,000,000 rows, inserted in one transaction. Binary columns are not copied.")
            };
            ui.label(RichText::new(note).small().color(p.muted));
            if masks.iter().any(|m| *m != Mask::Keep) {
                ui.label(
                    RichText::new(tr("Masking is deterministic: the same value always gives the same masked value, using a key kept in this computer's keychain."))
                        .small()
                        .color(p.muted),
                );
            }
            if to == Driver::Snowflake || self.source_driver == Driver::Snowflake {
                ui.colored_label(ui.visuals().warn_fg_color, tr("Snowflake: the copy runs on the warehouse and uses credits."));
            }
            if let Some(e) = &self.error {
                ui.add_space(8.0);
                ui.add(egui::Label::new(RichText::new(e).monospace().color(p.error)).wrap());
            }
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                if running {
                    ui.add(egui::Spinner::new().size(14.0).color(p.muted));
                    ui.label(RichText::new(tr("Copying…")).color(p.muted));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let ready = !running
                        && !columns.is_empty()
                        && !self.target_table.trim().is_empty()
                        && !matches!(target, Target::Loading)
                        && (to.flat_schema() || !self.target_schema.is_empty());
                    ui.add_enabled_ui(ready, |ui| {
                        if theme::primary_button(ui, tr("Copy")).clicked() {
                            result = Some(CopyAction::Start(Box::new(CopyRequest {
                                source_db: self.db.clone(),
                                source_schema: self.schema.clone(),
                                source_table: self.table.clone(),
                                columns: columns.clone(),
                                target_names: names.clone(),
                                filter: self.filter.trim().to_string(),
                                percent: self.percent,
                                target_db: self.target_db.clone(),
                                target_schema: self.target_schema.clone(),
                                target_table: self.target_table.trim().to_string(),
                                create: creating,
                                clear: self.clear && !creating,
                                masks: masks.clone(),
                                mask_key: String::new(),
                                parents: !mongo && (self.parents || self.children),
                                children: !mongo && self.children,
                                mask_related: !mongo && self.mask_related && (self.parents || self.children),
                            })));
                        }
                    });
                    if running {
                        if theme::secondary_button(ui, tr("Stop")).clicked() {
                            self.stop();
                        }
                    } else if theme::secondary_button(ui, tr("Cancel")).clicked() {
                        result = Some(CopyAction::Close);
                    }
                });
            });
        });
        if let Some(id) = new_target {
            self.pick_target(conns, id);
        }
        if !running && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            result = Some(CopyAction::Close);
        }
        result
    }

    fn columns_ui(&mut self, ui: &mut egui::Ui, target: &Target, to: Driver, p: &theme::Palette) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(tr("Columns")).font(theme::bold(13.0)));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(tr("Suggest masking")).clicked() {
                    self.suggest_masks();
                }
            });
        });
        ui.add_space(6.0);
        let driver = self.source_driver;
        egui::ScrollArea::vertical().id_salt("copy_columns").max_height(170.0).auto_shrink([false, true]).show(ui, |ui| {
            egui::Grid::new("copy_columns_grid").num_columns(3).spacing([12.0, 4.0]).show(ui, |ui| {
                for (i, ((c, on), mask)) in self.columns.iter().zip(self.selected.iter_mut()).zip(self.masks.iter_mut()).enumerate() {
                    let binary = copy::is_binary(self.source_driver, &c.data_type);
                    let missing = to != Driver::MongoDb && matches!(target, Target::Existing(cols) if !cols.iter().any(|t| t.name.eq_ignore_ascii_case(&c.name)));
                    ui.add_enabled_ui(!binary && !missing, |ui| {
                        ui.checkbox(on, &c.name);
                    });
                    let detail = if binary {
                        tr("binary: not copied").to_string()
                    } else if missing {
                        tr("not in the target table").to_string()
                    } else if matches!(target, Target::New) || to == Driver::MongoDb {
                        let ty = copy::target_type(self.source_driver, to, &c.data_type, c.primary_key);
                        if ty == c.data_type { ty.to_lowercase() } else { format!("{} → {ty}", c.data_type.to_lowercase()) }
                    } else {
                        c.data_type.to_lowercase()
                    };
                    ui.allocate_ui_with_layout(vec2(190.0, 18.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.set_min_width(190.0);
                        ui.add(egui::Label::new(RichText::new(detail).small().color(p.muted)).truncate());
                    });
                    if binary || missing {
                        ui.label("");
                    } else {
                        ui.add_enabled_ui(*on, |ui| {
                            egui::ComboBox::from_id_salt(("copy_mask", i)).selected_text(mask.label()).width(130.0).show_ui(ui, |ui| {
                                for m in Mask::ALL.into_iter().filter(|m| m.allowed(driver, c)) {
                                    ui.selectable_value(mask, m, m.label());
                                }
                            });
                        });
                    }
                    ui.end_row();
                }
            });
        });
    }
}

/// Connections that can receive a copy: connected, not read-only and not Redis, InfluxDB or
/// Kusto (no tables or collections to copy into, or rows are ingested rather than inserted).
fn writable(c: &ConnState) -> bool {
    c.status == ConnStatus::Connected && !c.cfg.read_only && c.cfg.driver.can_copy()
}
