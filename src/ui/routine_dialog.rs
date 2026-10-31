//! "New Procedure" / "New Function" / "Edit…" modal: name and schema, a parameter table, the
//! return type, the engine's options, the body in a highlighted editor and a live preview of
//! the statements it runs.

use crate::db::Driver;
use crate::db::routines::{
    self, Column, Param, ParamMode, ReturnKind, RoutineDefinition, RoutineKind, RoutineSpec, RoutineStatements,
};
use crate::i18n::{tr, trf};
use crate::sql;
use crate::ui::create_dialog::{common_types, sql_preview};
use crate::ui::icons::Ico;
use crate::ui::theme::{self, palette};
use crate::worker::ConnId;
use egui::{FontId, Margin, RichText, Ui, vec2};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tab {
    Parameters,
    Returns,
    Options,
}

pub struct RoutineDialog {
    pub conn: ConnId,
    pub driver: Driver,
    pub db: String,
    pub spec: RoutineSpec,
    /// Editing: the definition request in flight (`None` once it arrived).
    pub loading: Option<u64>,
    /// Statements sent to the worker; waiting for the reply.
    pub running: bool,
    pub error: Option<String>,
    /// The definition couldn't be split into fields: the statement is edited as text.
    pub raw: Option<String>,
    tab: Tab,
    focus_name: bool,
}

pub enum RoutineResult {
    Run(RoutineStatements),
    Cancel,
}

pub(crate) fn mode_label(driver: Driver, mode: ParamMode) -> &'static str {
    match (driver, mode) {
        (_, ParamMode::In) => "IN",
        (Driver::MsSql, ParamMode::Out) => "OUTPUT",
        (_, ParamMode::Out) => "OUT",
        (Driver::Oracle, ParamMode::InOut) => "IN OUT",
        (_, ParamMode::InOut) => "INOUT",
        (_, ParamMode::Variadic) => "VARIADIC",
    }
}

fn return_label(driver: Driver, kind: ReturnKind) -> &'static str {
    match (driver, kind) {
        (_, ReturnKind::Scalar) => tr("Value"),
        (_, ReturnKind::SetOf) => "SETOF",
        (Driver::MsSql, ReturnKind::Table) => tr("Table (table-valued function)"),
        (_, ReturnKind::Table) => tr("Table"),
    }
}

fn languages(driver: Driver) -> &'static [&'static str] {
    match driver {
        Driver::Postgres => &["plpgsql", "sql"],
        Driver::Snowflake => &["SQL", "JAVASCRIPT", "PYTHON", "JAVA", "SCALA"],
        _ => &[],
    }
}

impl RoutineDialog {
    pub fn new_routine(conn: ConnId, driver: Driver, db: String, schema: String, kind: RoutineKind) -> Self {
        Self {
            conn,
            driver,
            db,
            spec: RoutineSpec::new(driver, kind, schema),
            loading: None,
            running: false,
            error: None,
            raw: None,
            tab: Tab::Parameters,
            focus_name: true,
        }
    }

    /// Edits `spec.schema.name` (a placeholder until its definition arrives with `request_id`).
    pub fn edit_routine(conn: ConnId, driver: Driver, db: String, spec: RoutineSpec, request_id: u64) -> Self {
        Self { conn, driver, db, spec, loading: Some(request_id), running: false, error: None, raw: None, tab: Tab::Parameters, focus_name: false }
    }

    pub fn is_edit(&self) -> bool {
        self.spec.is_edit() || self.raw.is_some() || self.loading.is_some()
    }

    pub fn kind(&self) -> RoutineKind {
        self.spec.kind
    }

    /// The definition read from the server (or why it couldn't be).
    pub fn definition_loaded(&mut self, result: Result<RoutineDefinition, String>) {
        self.loading = None;
        match result {
            Ok(def) => match def.spec {
                Some(spec) => self.spec = spec,
                None => self.raw = Some(raw_statement(self.driver, &self.spec, &def.ddl)),
            },
            Err(e) => self.error = Some(e),
        }
    }

    pub fn statements(&self) -> RoutineStatements {
        if let Some(raw) = &self.raw {
            let statements = sql::split_batches(raw, self.driver).into_iter().map(|s| s.text).collect();
            return RoutineStatements { statements, transaction: false };
        }
        routines::routine_statements(self.driver, &self.spec)
    }

    fn preview_text(&self) -> String {
        let terminator = |s: &str| match self.driver {
            Driver::Oracle if s.trim_end().ends_with(';') => "\n/".to_string(),
            Driver::MsSql => "\nGO".to_string(),
            _ => ";".to_string(),
        };
        self.statements().statements.iter().map(|s| format!("{s}{}", terminator(s))).collect::<Vec<_>>().join("\n\n")
    }

    pub fn validate(&self) -> Result<RoutineStatements, String> {
        if self.raw.is_some() {
            return Ok(self.statements());
        }
        let s = &self.spec;
        let function = s.kind == RoutineKind::Function;
        if s.name.trim().is_empty() {
            return Err(tr("Name is required").into());
        }
        for (i, p) in s.params.iter().enumerate() {
            if p.name.trim().is_empty() && self.driver != Driver::Postgres {
                return Err(trf("Parameter {n} needs a name", &[("n", &(i + 1))]));
            }
            if p.data_type.trim().is_empty() {
                return Err(trf("Parameter {n} needs a type", &[("n", &(i + 1))]));
            }
        }
        let r = &s.returns;
        let needs_return = (function || self.driver == Driver::Snowflake) && !routines::return_kinds(self.driver, s.kind).is_empty();
        let pg_out = self.driver == Driver::Postgres && s.params.iter().any(|p| p.mode.returns_output());
        match r.kind {
            ReturnKind::Table if r.columns.is_empty() && self.driver != Driver::MsSql => return Err(tr("The returned table needs columns").into()),
            ReturnKind::Table if r.columns.iter().any(|c| c.name.trim().is_empty() || c.data_type.trim().is_empty()) => {
                return Err(tr("Every returned column needs a name and a type").into());
            }
            ReturnKind::Scalar | ReturnKind::SetOf if needs_return && !pg_out && r.data_type.trim().is_empty() => {
                return Err(tr("The return type is required").into());
            }
            _ => {}
        }
        if s.body.trim().is_empty() {
            return Err(tr("The body is required").into());
        }
        Ok(self.statements())
    }

    fn title(&self) -> &'static str {
        match (self.is_edit(), self.spec.kind) {
            (false, RoutineKind::Procedure) => tr("New Procedure"),
            (false, RoutineKind::Function) => tr("New Function"),
            (true, RoutineKind::Procedure) => tr("Edit Procedure"),
            (true, RoutineKind::Function) => tr("Edit Function"),
        }
    }

    /// `schemas`: the database's schemas, to choose from (empty if not loaded yet).
    pub fn show(&mut self, ctx: &egui::Context, schemas: &[String]) -> Option<RoutineResult> {
        let mut result = None;
        let p = palette(ctx.global_style().visuals.dark_mode);
        let edit = self.is_edit();
        let subtitle = trf("in {db}", &[("db", &self.db)]);
        super::workflow_dialog::show(ctx, egui::Id::new("routine_dialog"), self.title(), &subtitle, vec2(820.0, 740.0), |root, regions| {
            let mut body = regions.body_ui(root);
            let busy = self.running || self.loading.is_some();
            egui::ScrollArea::both().id_salt("routine_dialog_scroll").auto_shrink([false, false]).show(&mut body, |ui| {
                ui.add_enabled_ui(!busy, |ui| {
                    if let Some(raw) = self.raw.as_mut() {
                        ui.label(RichText::new(tr("The definition couldn't be split into fields: edit the statement directly.")).color(p.muted));
                        ui.add_space(6.0);
                        code_editor(ui, "routine_raw", self.driver, raw, 18, 420.0);
                        return;
                    }
                    self.form(ui, schemas);
                    ui.add_space(10.0);
                    self.tabs(ui);
                    ui.add_space(10.0);
                    ui.label(RichText::new(tr("Body:")).color(p.text));
                    ui.add_space(4.0);
                    code_editor(ui, "routine_body", self.driver, &mut self.spec.body, 10, 260.0);
                });
                if self.loading.is_some() {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(14.0).color(p.muted));
                        ui.label(RichText::new(tr("Loading the definition…")).color(p.muted));
                    });
                }
                if self.raw.is_none() {
                    ui.add_space(10.0);
                    if matches!(self.driver, Driver::Postgres | Driver::MsSql)
                        && self.spec.is_edit()
                        && self.statements().statements.iter().any(|s| s.starts_with("drop "))
                    {
                        ui.label(RichText::new(tr("Saving this change drops the routine's existing grants.")).color(p.warn));
                    }
                    sql_preview(ui, "routine", &self.preview_text());
                }
                if let Some(e) = &self.error {
                    ui.add_space(8.0);
                    egui::ScrollArea::vertical().id_salt("routine_error").max_height(110.0).show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(e).monospace().color(p.error)).wrap());
                    });
                }
            });
            let mut footer = regions.footer_ui(root);
            footer.horizontal(|ui| {
                if theme::secondary_button(ui, tr("Copy SQL")).clicked() {
                    ui.ctx().copy_text(self.preview_text());
                }
                if self.running {
                    ui.add_space(8.0);
                    ui.add(egui::Spinner::new().size(14.0).color(p.muted));
                    ui.label(RichText::new(if edit { tr("Saving…") } else { tr("Creating…") }).color(p.muted));
                }
            });
            footer.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.add_enabled_ui(!busy, |ui| {
                    if theme::primary_button(ui, if edit { tr("Save") } else { tr("Create") }).clicked() {
                        match self.validate() {
                            Ok(statements) => result = Some(RoutineResult::Run(statements)),
                            Err(e) => self.error = Some(e),
                        }
                    }
                });
                ui.add_enabled_ui(!self.running, |ui| {
                    if theme::secondary_button(ui, tr("Cancel")).clicked() {
                        result = Some(RoutineResult::Cancel);
                    }
                });
            });
        });
        if result.is_none() && !self.running && ctx.input(|i| i.key_pressed(egui::Key::Escape)) && !egui::Popup::is_any_open(ctx) {
            result = Some(RoutineResult::Cancel);
        }
        result
    }

    fn form(&mut self, ui: &mut Ui, schemas: &[String]) {
        let driver = self.driver;
        let edit = self.is_edit();
        egui::Grid::new("routine_form").num_columns(2).spacing([14.0, 10.0]).min_col_width(80.0).show(ui, |ui| {
            if !edit {
                ui.label(tr("Kind:"));
                ui.horizontal(|ui| {
                    let mut kind = self.spec.kind;
                    ui.selectable_value(&mut kind, RoutineKind::Procedure, tr("Procedure"));
                    ui.selectable_value(&mut kind, RoutineKind::Function, tr("Function"));
                    if kind != self.spec.kind {
                        self.switch_kind(kind);
                    }
                });
                ui.end_row();
            }
            ui.label(tr("Name:"));
            let name = ui.add(egui::TextEdit::singleline(&mut self.spec.name).margin(vec2(8.0, 5.0)).desired_width(f32::INFINITY));
            if std::mem::take(&mut self.focus_name) {
                name.request_focus();
            }
            ui.end_row();
            if !driver.flat_schema() {
                ui.label(tr("Schema:"));
                // A routine is edited (or renamed) within its schema.
                ui.add_enabled_ui(!edit, |ui| {
                    if schemas.is_empty() || edit {
                        ui.add(egui::TextEdit::singleline(&mut self.spec.schema).margin(vec2(8.0, 5.0)).desired_width(220.0));
                    } else {
                        egui::ComboBox::from_id_salt("routine_schema").selected_text(self.spec.schema.as_str()).width(220.0).show_ui(ui, |ui| {
                            for s in schemas {
                                ui.selectable_value(&mut self.spec.schema, s.clone(), s);
                            }
                        });
                    }
                });
                ui.end_row();
            }
            if !edit {
                ui.label(tr("Options:"));
                let label = match driver {
                    Driver::MsSql => "OR ALTER",
                    Driver::MySql => "DROP IF EXISTS",
                    _ => "OR REPLACE",
                };
                ui.checkbox(&mut self.spec.or_replace, RichText::new(label).monospace());
                ui.end_row();
            }
        });
    }

    /// Switches a new routine between procedure and function, keeping what was typed.
    fn switch_kind(&mut self, kind: RoutineKind) {
        let old = RoutineSpec::new(self.driver, self.spec.kind, self.spec.schema.clone());
        let fresh = RoutineSpec::new(self.driver, kind, self.spec.schema.clone());
        let modes = routines::param_modes(self.driver, kind);
        let mut spec = RoutineSpec { name: std::mem::take(&mut self.spec.name), or_replace: self.spec.or_replace, ..fresh.clone() };
        spec.params = std::mem::take(&mut self.spec.params)
            .into_iter()
            .map(|mut p| {
                if !modes.contains(&p.mode) {
                    p.mode = ParamMode::In;
                }
                p
            })
            .collect();
        if self.spec.body != old.body {
            spec.body = std::mem::take(&mut self.spec.body);
        }
        if self.spec.returns != old.returns && !routines::return_kinds(self.driver, kind).is_empty() {
            spec.returns = std::mem::take(&mut self.spec.returns);
        }
        if self.spec.options != old.options {
            spec.options = std::mem::take(&mut self.spec.options);
        }
        self.spec = spec;
        if self.tab == Tab::Returns && routines::return_kinds(self.driver, kind).is_empty() {
            self.tab = Tab::Parameters;
        }
    }

    fn tabs(&mut self, ui: &mut Ui) {
        let has_returns = !routines::return_kinds(self.driver, self.spec.kind).is_empty();
        ui.horizontal(|ui| {
            for (tab, label) in [(Tab::Parameters, tr("Parameters")), (Tab::Returns, tr("Returns")), (Tab::Options, tr("Options"))] {
                if tab == Tab::Returns && !has_returns {
                    continue;
                }
                ui.selectable_value(&mut self.tab, tab, RichText::new(label).font(theme::bold(13.0)));
            }
        });
        ui.add_space(8.0);
        match self.tab {
            Tab::Parameters => self.parameters(ui),
            Tab::Returns if has_returns => self.returns(ui),
            Tab::Options => self.options(ui),
            Tab::Returns => self.tab = Tab::Parameters,
        }
    }

    fn parameters(&mut self, ui: &mut Ui) {
        let p = theme::pal(ui);
        let driver = self.driver;
        let modes = routines::param_modes(driver, self.spec.kind);
        let defaults = routines::has_defaults(driver);
        let params = &mut self.spec.params;
        let mut remove = None;
        let mut swap = None;
        if params.is_empty() {
            ui.label(RichText::new(tr("No parameters.")).color(p.muted));
        } else {
            egui::ScrollArea::vertical().id_salt("routine_params").max_height(200.0).auto_shrink([false, true]).show(ui, |ui| {
                egui::Grid::new("routine_params_grid").num_columns(6).spacing([8.0, 6.0]).show(ui, |ui| {
                    for h in [tr("Name"), tr("Mode"), tr("Type"), if defaults { tr("Default") } else { "" }, "", ""] {
                        ui.label(RichText::new(h).small().color(p.muted));
                    }
                    ui.end_row();
                    let n = params.len();
                    for (i, param) in params.iter_mut().enumerate() {
                        let hint = if driver == Driver::MsSql { "@name" } else { "" };
                        ui.add_sized([150.0, 26.0], egui::TextEdit::singleline(&mut param.name).id_salt(("param_name", i)).hint_text(hint).margin(vec2(6.0, 4.0)));
                        ui.add_enabled_ui(modes.len() > 1, |ui| {
                            egui::ComboBox::from_id_salt(("param_mode", i)).selected_text(mode_label(driver, param.mode)).width(90.0).show_ui(ui, |ui| {
                                for m in modes {
                                    ui.selectable_value(&mut param.mode, *m, mode_label(driver, *m));
                                }
                            });
                        });
                        type_field(ui, ("param_type", i), driver, &mut param.data_type, 170.0);
                        if defaults {
                            ui.add_sized(
                                [130.0, 26.0],
                                egui::TextEdit::singleline(&mut param.default).id_salt(("param_default", i)).font(egui::TextStyle::Monospace).margin(vec2(6.0, 4.0)),
                            );
                        } else {
                            ui.label("");
                        }
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 2.0;
                            if theme::icon_button(ui, Ico::SortAsc, i > 0, tr("Move up")).clicked() {
                                swap = Some((i - 1, i));
                            }
                            if theme::icon_button(ui, Ico::SortDesc, i + 1 < n, tr("Move down")).clicked() {
                                swap = Some((i, i + 1));
                            }
                        });
                        if theme::icon_button(ui, Ico::Minus, true, tr("Remove parameter")).clicked() {
                            remove = Some(i);
                        }
                        ui.end_row();
                    }
                });
            });
        }
        if let Some((a, b)) = swap {
            params.swap(a, b);
        }
        if let Some(i) = remove {
            params.remove(i);
        }
        ui.add_space(4.0);
        if theme::flat_button(ui, Some(Ico::Plus), tr("Add Parameter"), FontId::proportional(13.0), false, true).clicked() {
            let n = params.len() + 1;
            let name = if driver == Driver::MsSql { format!("@p{n}") } else { format!("p{n}") };
            let data_type = common_types(driver).first().copied().unwrap_or_default().to_string();
            params.push(Param { name, data_type, ..Default::default() });
        }
    }

    fn returns(&mut self, ui: &mut Ui) {
        let p = theme::pal(ui);
        let driver = self.driver;
        let kinds = routines::return_kinds(driver, self.spec.kind);
        let r = &mut self.spec.returns;
        if kinds.len() > 1 {
            ui.horizontal(|ui| {
                for k in kinds {
                    ui.radio_value(&mut r.kind, *k, return_label(driver, *k));
                }
            });
            ui.add_space(6.0);
        }
        match r.kind {
            ReturnKind::Scalar | ReturnKind::SetOf => {
                ui.horizontal(|ui| {
                    ui.label(tr("Type:"));
                    type_field(ui, "return_type", driver, &mut r.data_type, 240.0);
                });
                if driver == Driver::Postgres && self.spec.params.iter().any(|p| p.mode.returns_output()) {
                    ui.label(RichText::new(tr("With OUT parameters the type can be left empty (it is a record).")).small().color(p.muted));
                }
            }
            ReturnKind::Table => {
                if driver == Driver::MsSql {
                    ui.horizontal(|ui| {
                        ui.label(tr("Table variable:"));
                        ui.add(egui::TextEdit::singleline(&mut r.table_variable).hint_text("@result").desired_width(140.0).margin(vec2(6.0, 4.0)));
                    });
                    ui.label(RichText::new(tr("Without columns it is an inline function whose body is RETURN (SELECT …).")).small().color(p.muted));
                    ui.add_space(4.0);
                }
                columns_editor(ui, driver, &mut r.columns);
            }
        }
    }

    fn options(&mut self, ui: &mut Ui) {
        let driver = self.driver;
        let function = self.spec.kind == RoutineKind::Function;
        let inline = driver == Driver::MsSql && self.spec.returns.kind == ReturnKind::Table && self.spec.returns.columns.is_empty();
        let scalar = self.spec.returns.kind == ReturnKind::Scalar;
        let o = &mut self.spec.options;
        egui::Grid::new("routine_options").num_columns(2).spacing([14.0, 8.0]).min_col_width(120.0).show(ui, |ui| {
            let langs = languages(driver);
            if !langs.is_empty() {
                ui.label(tr("Language:"));
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut o.language).desired_width(160.0).margin(vec2(6.0, 4.0)));
                    let pick = theme::icon_button(ui, Ico::ChevronDown, true, tr("Choose a language"));
                    egui::Popup::menu(&pick).show(|ui| {
                        for l in langs {
                            if ui.button(*l).clicked() {
                                o.language = (*l).to_string();
                                ui.close();
                            }
                        }
                    });
                });
                ui.end_row();
            }
            match driver {
                Driver::Postgres => {
                    if function {
                        choice(ui, tr("Volatility:"), "pg_volatility", &mut o.volatility, &["", "VOLATILE", "STABLE", "IMMUTABLE"]);
                        choice(ui, tr("Parallel:"), "pg_parallel", &mut o.parallel, &["", "SAFE", "RESTRICTED", "UNSAFE"]);
                        flag(ui, "STRICT", &mut o.strict, tr("Returns NULL when any argument is NULL"));
                    }
                    choice(ui, tr("Security:"), "pg_security", &mut o.security, &["", "INVOKER", "DEFINER"]);
                    text_option(ui, tr("Comment:"), &mut o.comment);
                }
                Driver::MySql | Driver::MariaDb => {
                    text_option(ui, "DEFINER:", &mut o.definer);
                    flag(ui, "DETERMINISTIC", &mut o.deterministic, tr("Same arguments always give the same result"));
                    choice(ui, tr("Data access:"), "my_access", &mut o.data_access, &["", "CONTAINS SQL", "NO SQL", "READS SQL DATA", "MODIFIES SQL DATA"]);
                    choice(ui, "SQL SECURITY:", "my_security", &mut o.security, &["", "DEFINER", "INVOKER"]);
                    text_option(ui, tr("Comment:"), &mut o.comment);
                }
                Driver::MsSql => {
                    if !inline {
                        choice(ui, "EXECUTE AS:", "ms_execute", &mut o.security, &["", "CALLER", "OWNER", "SELF"]);
                    }
                    if function {
                        flag(ui, "SCHEMABINDING", &mut o.schemabinding, tr("Binds the function to the objects it uses"));
                        if scalar {
                            flag(ui, "RETURNS NULL ON NULL INPUT", &mut o.strict, tr("Returns NULL when any argument is NULL"));
                        }
                    } else {
                        flag(ui, "RECOMPILE", &mut o.recompile, tr("Compiles a new plan on every run"));
                    }
                }
                Driver::Oracle => {
                    choice(ui, "AUTHID:", "ora_authid", &mut o.security, &["", "DEFINER", "CURRENT_USER"]);
                    if function {
                        flag(ui, "DETERMINISTIC", &mut o.deterministic, tr("Same arguments always give the same result"));
                        flag(ui, "PARALLEL_ENABLE", &mut o.parallel_enable, tr("Can run in parallel queries"));
                        flag(ui, "PIPELINED", &mut o.pipelined, tr("Returns rows as they are produced (a collection type)"));
                    }
                }
                Driver::Snowflake => {
                    flag(ui, "SECURE", &mut o.secure, tr("Hides the definition from other roles"));
                    choice(ui, tr("Volatility:"), "sf_volatility", &mut o.volatility, &["", "VOLATILE", "IMMUTABLE"]);
                    flag(ui, "RETURNS NULL ON NULL INPUT", &mut o.strict, tr("Returns NULL when any argument is NULL"));
                    if !function {
                        choice(ui, "EXECUTE AS:", "sf_execute", &mut o.security, &["", "OWNER", "CALLER", "RESTRICTED CALLER"]);
                    }
                    text_option(ui, tr("Comment:"), &mut o.comment);
                    let lang = o.language.to_ascii_uppercase();
                    if !matches!(lang.as_str(), "" | "SQL" | "JAVASCRIPT") {
                        text_option(ui, "RUNTIME_VERSION:", &mut o.runtime_version);
                        text_option(ui, "PACKAGES:", &mut o.packages);
                        text_option(ui, "HANDLER:", &mut o.handler);
                    }
                }
                _ => {}
            }
            ui.label(tr("Other clauses:"));
            ui.add(
                egui::TextEdit::multiline(&mut o.extra)
                    .font(egui::TextStyle::Monospace)
                    .hint_text(match driver {
                        Driver::Postgres => "COST 100\nSET search_path = public",
                        Driver::Snowflake => "IMPORTS = ('@stage/lib.py')",
                        _ => "",
                    })
                    .desired_rows(2)
                    .desired_width(f32::INFINITY),
            );
            ui.end_row();
        });
    }
}

/// The statement shown when the definition can't be split into fields, written so running it
/// replaces the routine.
fn raw_statement(driver: Driver, spec: &RoutineSpec, ddl: &str) -> String {
    let ddl = ddl.trim();
    let replace = |from: &str, to: &str| {
        let lower = ddl.to_ascii_lowercase();
        if lower.starts_with(from) && !lower.starts_with(to) { format!("{to}{}", &ddl[from.len()..]) } else { ddl.to_string() }
    };
    match driver {
        Driver::MsSql => replace("create ", "create or alter ").trim_end_matches("GO").trim_end().to_string(),
        Driver::MariaDb => replace("create ", "create or replace "),
        Driver::MySql => format!(
            "drop {} if exists {};\n{ddl}",
            spec.kind.keyword(),
            crate::db::Session::quote_table(driver, &spec.schema, &spec.name)
        ),
        _ => ddl.to_string(),
    }
}

/// A type: free text plus the engine's usual types.
fn type_field(ui: &mut Ui, id: impl std::hash::Hash + std::fmt::Debug + Copy, driver: Driver, value: &mut String, width: f32) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        ui.add_sized([width, 26.0], egui::TextEdit::singleline(value).id_salt(("type", id)).font(egui::TextStyle::Monospace).margin(vec2(6.0, 4.0)));
        let pick = theme::icon_button(ui, Ico::ChevronDown, true, tr("Choose a type"));
        egui::Popup::menu(&pick).show(|ui| {
            egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                for t in common_types(driver) {
                    if ui.button(RichText::new(*t).monospace()).clicked() {
                        *value = (*t).to_string();
                        ui.close();
                    }
                }
            });
        });
    });
}

/// Columns of a returned table.
fn columns_editor(ui: &mut Ui, driver: Driver, columns: &mut Vec<Column>) {
    let p = theme::pal(ui);
    let mut remove = None;
    if !columns.is_empty() {
        egui::Grid::new("routine_columns_grid").num_columns(3).spacing([8.0, 6.0]).show(ui, |ui| {
            for h in [tr("Column"), tr("Type"), ""] {
                ui.label(RichText::new(h).small().color(p.muted));
            }
            ui.end_row();
            for (i, c) in columns.iter_mut().enumerate() {
                ui.add_sized([160.0, 26.0], egui::TextEdit::singleline(&mut c.name).id_salt(("ret_col", i)).margin(vec2(6.0, 4.0)));
                type_field(ui, ("ret_type", i), driver, &mut c.data_type, 170.0);
                if theme::icon_button(ui, Ico::Minus, true, tr("Remove column")).clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
    }
    if let Some(i) = remove {
        columns.remove(i);
    }
    ui.add_space(4.0);
    if theme::flat_button(ui, Some(Ico::Plus), tr("Add Column"), FontId::proportional(13.0), false, true).clicked() {
        let data_type = common_types(driver).first().copied().unwrap_or_default().to_string();
        columns.push(Column { name: format!("c{}", columns.len() + 1), data_type });
    }
}

/// A grid row with a choice whose empty value means "the engine's default".
fn choice(ui: &mut Ui, label: &str, id: &str, value: &mut String, choices: &[&str]) {
    let shown = |v: &str| if v.is_empty() { tr("Default").to_string() } else { v.to_string() };
    ui.label(label);
    egui::ComboBox::from_id_salt(id).selected_text(shown(value)).width(180.0).show_ui(ui, |ui| {
        for c in choices {
            ui.selectable_value(value, (*c).to_string(), shown(c));
        }
    });
    ui.end_row();
}

fn flag(ui: &mut Ui, keyword: &str, value: &mut bool, hint: &str) {
    ui.label("");
    ui.checkbox(value, RichText::new(keyword).monospace()).on_hover_text(hint);
    ui.end_row();
}

fn text_option(ui: &mut Ui, label: &str, value: &mut String) {
    ui.label(label);
    ui.add(egui::TextEdit::singleline(value).desired_width(f32::INFINITY).margin(vec2(6.0, 4.0)));
    ui.end_row();
}

/// Highlighted SQL editor.
fn code_editor(ui: &mut Ui, id: &str, driver: Driver, text: &mut String, rows: usize, max_height: f32) {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    egui::Frame::NONE
        .fill(p.editor_bg)
        .stroke(egui::Stroke::new(1.0, p.control_border))
        .corner_radius(6)
        .inner_margin(Margin::same(8))
        .show(ui, |ui| {
            egui::ScrollArea::both().id_salt((id, "scroll")).max_height(max_height).auto_shrink([false, true]).show(ui, |ui| {
                let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, _w: f32| {
                    let mut job = sql::highlight_for(buf.as_str(), driver, dark, FontId::monospace(13.0));
                    job.wrap.max_width = f32::INFINITY;
                    ui.fonts_mut(|f| f.layout_job(job))
                };
                ui.add(
                    egui::TextEdit::multiline(text)
                        .id(egui::Id::new((id, "sql")))
                        .code_editor()
                        .frame(egui::Frame::NONE)
                        .desired_width(f32::INFINITY)
                        .desired_rows(rows)
                        .layouter(&mut layouter),
                );
            });
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn footer_actions_stay_visible_with_long_definition_errors() {
        use egui_kittest::kittest::Queryable;
        for size in [vec2(780.0, 560.0), vec2(400.0, 300.0)] {
            for dark in [false, true] {
                let mut dialog = RoutineDialog::new_routine(1, Driver::Postgres, "shop_".repeat(50), "public".into(), RoutineKind::Function);
                dialog.error = Some("Definition failed with additional diagnostic context.\n".repeat(80));
                let mut first = true;
                let mut h = egui_kittest::Harness::builder().with_size(size).build_ui_state(
                    move |ui, dialog: &mut RoutineDialog| {
                        if std::mem::take(&mut first) {
                            theme::install_fonts(ui.ctx());
                            theme::apply(ui.ctx(), dark);
                        } else {
                            dialog.show(ui.ctx(), &[]);
                        }
                    },
                    dialog,
                );
                h.run_steps(3);
                let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
                for label in ["Copy SQL", "Create", "Cancel"] {
                    assert!(screen.contains_rect(h.get_by_label(label).rect()), "{label}");
                }
            }
        }
    }

    #[test]
    fn validation() {
        let mut d = RoutineDialog::new_routine(1, Driver::Postgres, "db".into(), "public".into(), RoutineKind::Function);
        assert_eq!(d.validate().unwrap_err(), tr("Name is required"));
        d.spec.name = "f".into();
        d.spec.params.push(Param { name: "a".into(), data_type: String::new(), ..Default::default() });
        assert!(d.validate().is_err());
        d.spec.params[0].data_type = "int".into();
        d.spec.returns.data_type.clear();
        assert_eq!(d.validate().unwrap_err(), tr("The return type is required"));
        d.spec.params.push(Param { name: "b".into(), mode: ParamMode::Out, data_type: "int".into(), ..Default::default() });
        assert!(d.validate().is_ok(), "OUT parameters make it a record");
        d.spec.returns.kind = ReturnKind::Table;
        assert_eq!(d.validate().unwrap_err(), tr("The returned table needs columns"));

        let mut ms = RoutineDialog::new_routine(1, Driver::MsSql, "db".into(), "dbo".into(), RoutineKind::Function);
        ms.spec.name = "tvf".into();
        ms.spec.returns.kind = ReturnKind::Table;
        ms.spec.body = "return (select 1 as one)".into();
        let st = ms.validate().unwrap();
        assert!(st.statements[0].contains("returns table\nas\nreturn (select 1 as one)"), "{}", st.statements[0]);
    }

    struct State {
        dialog: RoutineDialog,
        created: Option<RoutineStatements>,
    }

    fn harness(dialog: RoutineDialog) -> egui_kittest::Harness<'static, State> {
        harness_with_size(dialog, vec2(1100.0, 900.0), true)
    }

    fn harness_with_size(dialog: RoutineDialog, size: egui::Vec2, dark: bool) -> egui_kittest::Harness<'static, State> {
        let mut first = true;
        let mut h = egui_kittest::Harness::builder().with_size(size).build_ui_state(
            move |ui, s: &mut State| {
                let ctx = ui.ctx().clone();
                if std::mem::take(&mut first) {
                    theme::install_fonts(&ctx);
                    theme::apply(&ctx, dark);
                } else if let Some(RoutineResult::Run(st)) = s.dialog.show(&ctx, &["public".into(), "app".into()]) {
                    s.created = Some(st);
                }
            },
            State { dialog, created: None },
        );
        h.run_steps(3);
        h
    }

    /// Every engine's form renders, its tabs open and a parameter typed in the table reaches
    /// the statement that runs.
    #[test]
    fn the_form_drives_the_statement_for_every_engine() {
        use egui_kittest::kittest::Queryable;
        for driver in [Driver::Postgres, Driver::MySql, Driver::MariaDb, Driver::MsSql, Driver::Oracle, Driver::Snowflake] {
            for kind in [RoutineKind::Procedure, RoutineKind::Function] {
                let schema = if driver.flat_schema() { "shop" } else { "public" };
                let mut h = harness(RoutineDialog::new_routine(1, driver, "shop".into(), schema.into(), kind));
                h.state_mut().dialog.spec.name = "touch".into();
                h.get_by_label(tr("Add Parameter")).click_accesskit();
                h.run_steps(3);
                assert_eq!(h.state().dialog.spec.params.len(), 1, "{driver:?} {kind:?}");
                if h.query_by_label(tr("Returns")).is_some() {
                    h.get_by_label(tr("Returns")).click_accesskit();
                    h.run_steps(3);
                }
                h.get_by_label(tr("Options")).click_accesskit();
                h.run_steps(3);
                assert!(h.query_by_label(tr("Other clauses:")).is_some(), "{driver:?} {kind:?}");
                h.get_by_label(tr("Create")).click_accesskit();
                h.run_steps(3);
                let created = h.state().created.clone().unwrap_or_else(|| panic!("{driver:?} {kind:?}: {:?}", h.state().dialog.error));
                let sql = created.statements[0].to_lowercase();
                assert!(sql.contains("touch"), "{driver:?} {kind:?}: {created:?}");
                assert!(sql.contains(if driver == Driver::MsSql { "@p1" } else { "p1" }), "{driver:?} {kind:?}: {created:?}");
            }
        }
    }

    #[test]
    fn warns_when_edit_drops_existing_grants() {
        use egui_kittest::kittest::Queryable;
        let mut spec = RoutineSpec::new(Driver::Postgres, RoutineKind::Function, "public".into());
        spec.name = "total".into();
        spec.mark_original("integer");
        let mut h = harness(RoutineDialog::edit_routine(1, Driver::Postgres, "shop".into(), spec, 1));
        h.state_mut().dialog.loading = None;
        h.run_steps(3);
        assert!(h.query_by_label(tr("Saving this change drops the routine's existing grants.")).is_none());
        h.state_mut().dialog.spec.returns.data_type = "bigint".into();
        h.run_steps(3);
        assert!(h.query_by_label(tr("Saving this change drops the routine's existing grants.")).is_some());
    }

    #[test]
    #[ignore = "Requires a GPU; writes routine dialog previews to target/routine-snapshots"]
    fn render_routine_dialogs() {
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/routine-snapshots");
        std::fs::create_dir_all(&directory).unwrap();
        let function = || {
            let mut d = RoutineDialog::new_routine(1, Driver::Postgres, "shop".into(), "public".into(), RoutineKind::Function);
            d.spec.name = "order_total".into();
            d.spec.params = vec![
                Param { name: "order_id".into(), data_type: "integer".into(), ..Default::default() },
                Param { name: "tax_rate".into(), data_type: "numeric(5,2)".into(), default: "0.23".into(), ..Default::default() },
            ];
            d.spec.returns.data_type = "numeric(12,2)".into();
            d.spec.options.volatility = "STABLE".into();
            d.spec.body = "begin\n    return (select sum(price * qty) from order_lines l where l.order_id = order_total.order_id)\n         * (1 + tax_rate);\nend;".into();
            d
        };
        for dark in [false, true] {
            for (tab, name) in [(Tab::Parameters, "new-function"), (Tab::Returns, "new-function-returns"), (Tab::Options, "new-function-options")] {
                let mut h = harness_with_size(RoutineDialog { tab, ..function() }, vec2(780.0, 560.0), dark);
                h.run_steps(3);
                h.render().unwrap().save(directory.join(format!("{name}-{}.png", if dark { "dark" } else { "light" }))).unwrap();
            }
            let ms = crate::db::routines::parse_definition(
            Driver::MsSql,
            "create procedure app.add_order\n    @user_id int,\n    @note nvarchar(20) = N'none',\n    @new_id bigint output\nwith recompile\nas\nbegin\n    set nocount on;\n    insert orders (user_id, note) values (@user_id, @note);\n    set @new_id = scope_identity();\nend",
        )
        .unwrap();
            let mut edit = RoutineDialog::edit_routine(1, Driver::MsSql, "shop".into(), ms.clone(), 1);
            let mut loaded = ms;
            loaded.mark_original("");
            edit.definition_loaded(Ok(RoutineDefinition { ddl: String::new(), spec: Some(loaded) }));
            let mut h = harness_with_size(edit, vec2(780.0, 560.0), dark);
            h.run_steps(3);
            h.render().unwrap().save(directory.join(format!("edit-procedure-{}.png", if dark { "dark" } else { "light" }))).unwrap();
        }
    }

    #[test]
    fn switching_kind_keeps_what_was_typed() {
        let mut d = RoutineDialog::new_routine(1, Driver::MySql, "shop".into(), "shop".into(), RoutineKind::Procedure);
        d.spec.name = "p".into();
        d.spec.params.push(Param { name: "x".into(), mode: ParamMode::Out, data_type: "int".into(), ..Default::default() });
        d.switch_kind(RoutineKind::Function);
        assert_eq!(d.spec.name, "p");
        assert_eq!(d.spec.params[0].mode, ParamMode::In, "MySQL function parameters have no mode");
        assert_eq!(d.spec.returns.data_type, "int");
        assert_eq!(d.spec.body, "begin\n    return 0;\nend");
        d.spec.body = "return 1".into();
        d.switch_kind(RoutineKind::Procedure);
        assert_eq!(d.spec.body, "return 1");
    }

    #[test]
    fn definitions_that_cant_be_split_are_edited_as_text() {
        let mut d = RoutineDialog::edit_routine(1, Driver::MsSql, "shop".into(), RoutineSpec { name: "p".into(), schema: "dbo".into(), ..Default::default() }, 3);
        d.definition_loaded(Ok(RoutineDefinition { ddl: "CREATE PROCEDURE dbo.p WITH ENCRYPTION AS SELECT 1\nGO\n".into(), spec: None }));
        assert_eq!(d.raw.as_deref(), Some("create or alter PROCEDURE dbo.p WITH ENCRYPTION AS SELECT 1"));
        assert_eq!(d.validate().unwrap().statements, vec!["create or alter PROCEDURE dbo.p WITH ENCRYPTION AS SELECT 1"]);
        assert!(d.is_edit());

        let mut my = RoutineDialog::edit_routine(1, Driver::MySql, "shop".into(), RoutineSpec { name: "p".into(), schema: "shop".into(), ..Default::default() }, 3);
        my.definition_loaded(Ok(RoutineDefinition { ddl: "CREATE PROCEDURE `p`() SELECT 1;\n".into(), spec: None }));
        assert_eq!(my.statements().statements, vec!["drop procedure if exists `shop`.`p`", "CREATE PROCEDURE `p`() SELECT 1"]);
    }
}
