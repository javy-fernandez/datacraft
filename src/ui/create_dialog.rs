//! Modal dialogs to create a database or a table, with a live preview of the generated SQL.

use crate::db::Driver;
use crate::i18n::{tr, trf};
use crate::sql;
use crate::ui::icons::Ico;
use crate::ui::quote_ident_if_needed;
use crate::ui::theme::{self, palette};
use crate::worker::ConnId;
use egui::{FontId, Margin, RichText, Ui, vec2};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewColumn {
    pub name: String,
    pub data_type: String,
    pub not_null: bool,
    pub primary_key: bool,
    /// SQL expression, written as is (e.g. `0`, `'n/a'`, `now()`).
    pub default: String,
}

pub enum CreateTarget {
    Database,
    /// Table in `db`, inside `schema` (ignored on SQLite).
    Table { db: String, schema: String },
}

pub struct CreateDialog {
    pub conn: ConnId,
    pub driver: Driver,
    pub target: CreateTarget,
    pub name: String,
    pub columns: Vec<NewColumn>,
    /// Statements sent to the worker; waiting for the reply.
    pub running: bool,
    pub error: Option<String>,
    /// "New Database": the options, and the server's choices for them (None while loading).
    pub db_options: crate::db::create_db::DatabaseOptions,
    pub choices: Option<crate::db::create_db::DatabaseChoices>,
    /// Focus the name field on the first frame.
    focus_name: bool,
}

pub enum CreateResult {
    Create(Vec<String>),
    Cancel,
}

/// Usual types per engine, offered in each column's type menu.
pub(crate) fn common_types(driver: Driver) -> &'static [&'static str] {
    match driver {
        Driver::Postgres => &[
            "integer", "bigint", "smallint", "serial", "bigserial", "numeric(10,2)", "real", "double precision",
            "boolean", "varchar(255)", "text", "date", "timestamp", "timestamptz", "uuid", "jsonb", "bytea",
        ],
        Driver::MsSql => &[
            "int", "bigint", "smallint", "tinyint", "bit", "decimal(18,2)", "float", "money", "nvarchar(255)",
            "nvarchar(max)", "varchar(255)", "date", "datetime2", "uniqueidentifier", "varbinary(max)",
        ],
        Driver::Sqlite => &["INTEGER", "TEXT", "REAL", "NUMERIC", "BLOB", "BOOLEAN", "DATE", "DATETIME"],
        Driver::MySql | Driver::MariaDb => &[
            "int", "bigint", "smallint", "tinyint", "decimal(10,2)", "double", "boolean", "varchar(255)", "text",
            "longtext", "date", "datetime", "timestamp", "json", "blob",
        ],
        Driver::Oracle => &[
            "number", "number(10)", "number(10,2)", "binary_double", "varchar2(255)", "nvarchar2(255)", "char(1)",
            "clob", "date", "timestamp", "blob", "raw(16)",
        ],
        Driver::Redis | Driver::MongoDb => &[],
        Driver::ClickHouse => &[
            "UInt8", "UInt16", "UInt32", "UInt64", "Int8", "Int16", "Int32", "Int64", "Float32", "Float64",
            "Decimal(18, 2)", "String", "UUID", "Date", "DateTime", "DateTime64(3)", "Bool", "LowCardinality(String)",
            "Nullable(String)", "Array(String)",
        ],
        Driver::Snowflake => &[
            "number(38,0)", "number(10,2)", "float", "boolean", "varchar", "varchar(255)", "date", "time",
            "timestamp_ntz", "timestamp_ltz", "timestamp_tz", "variant", "object", "array", "binary",
        ],
    }
}

/// Schema used for new tables when none is selected.
pub fn default_schema(driver: Driver) -> &'static str {
    match driver {
        Driver::Postgres => "public",
        Driver::MsSql => "dbo",
        Driver::Sqlite => "main",
        Driver::Snowflake => "PUBLIC",
        Driver::Redis | Driver::MongoDb => "",
        // ClickHouse has no schemas: the (only) database is the schema shown.
        Driver::ClickHouse => "default",
        // MySQL: the database itself; Oracle: the connected user (filled in by the caller).
        Driver::MySql | Driver::MariaDb | Driver::Oracle => "",
    }
}

fn id_column(driver: Driver) -> NewColumn {
    let data_type = match driver {
        Driver::Postgres => "integer",
        Driver::MsSql => "int",
        Driver::Sqlite => "INTEGER",
        Driver::MySql | Driver::MariaDb => "int",
        Driver::Oracle => "number(10)",
        Driver::Snowflake => "number(38,0)",
        Driver::ClickHouse => "UInt64",
        Driver::Redis | Driver::MongoDb => "string",
    };
    NewColumn { name: "id".into(), data_type: data_type.into(), not_null: true, primary_key: true, default: String::new() }
}


pub fn create_table_sql(driver: Driver, schema: &str, table: &str, columns: &[NewColumn]) -> String {
    let q = |s: &str| quote_ident_if_needed(driver, s.trim());
    let qualified = if driver == Driver::Sqlite || schema.trim().is_empty() {
        q(table)
    } else {
        format!("{}.{}", q(schema), q(table))
    };
    let mut lines: Vec<String> = columns
        .iter()
        .map(|c| {
            if driver == Driver::ClickHouse {
                let mut l = format!("    {} {}", q(&c.name), crate::db::clickhouse::column_type(&c.data_type, c.not_null || c.primary_key));
                if !c.default.trim().is_empty() {
                    l.push_str(&format!(" default {}", c.default.trim()));
                }
                return l;
            }
            let mut l = format!("    {} {}", q(&c.name), c.data_type.trim());
            if c.not_null || c.primary_key {
                l.push_str(" not null");
            }
            if !c.default.trim().is_empty() {
                l.push_str(&format!(" default {}", c.default.trim()));
            }
            l
        })
        .collect();
    let pk: Vec<String> = columns.iter().filter(|c| c.primary_key).map(|c| q(&c.name)).collect();
    if !pk.is_empty() {
        lines.push(format!("    primary key ({})", pk.join(", ")));
    }
    let mut sql = format!("create table {qualified}\n(\n{}\n)", lines.join(",\n"));
    if driver == Driver::ClickHouse {
        // Mandatory: without an ENGINE clause the server uses its `default_table_engine`
        // (Memory: the data would vanish on restart). MergeTree with the PK as ORDER BY.
        let order = if pk.is_empty() { "tuple()".to_string() } else { pk.join(", ") };
        sql.push_str(&format!("\nengine = MergeTree\norder by ({order})"));
    }
    sql
}

/// Demo mode: sample columns for the "New Table" dialog.
pub fn demo_columns() -> Vec<NewColumn> {
    vec![
        NewColumn { name: "name".into(), data_type: "TEXT".into(), not_null: true, primary_key: false, default: String::new() },
        NewColumn { name: "active".into(), data_type: "BOOLEAN".into(), not_null: false, primary_key: false, default: "1".into() },
    ]
}

impl CreateDialog {
    pub fn database(conn: ConnId, driver: Driver) -> Self {
        Self {
            conn,
            driver,
            target: CreateTarget::Database,
            name: String::new(),
            columns: vec![],
            running: false,
            error: None,
            db_options: Default::default(),
            choices: None,
            focus_name: true,
        }
    }

    pub fn table(conn: ConnId, driver: Driver, db: String, schema: String) -> Self {
        Self {
            conn,
            driver,
            target: CreateTarget::Table { db, schema },
            name: String::new(),
            columns: vec![id_column(driver)],
            running: false,
            error: None,
            db_options: Default::default(),
            choices: None,
            focus_name: true,
        }
    }

    pub fn sql(&self) -> String {
        match &self.target {
            CreateTarget::Database => {
                let mariadb = self.choices.as_ref().is_some_and(|c| c.mariadb);
                crate::db::create_db::create_database_sql(self.driver, &self.name, &self.db_options, mariadb)
            }
            CreateTarget::Table { schema, .. } => create_table_sql(self.driver, schema, &self.name, &self.columns),
        }
    }

    fn validate(&self) -> Result<Vec<String>, String> {
        if self.name.trim().is_empty() {
            return Err(tr("Name is required").into());
        }
        if let CreateTarget::Table { .. } = self.target {
            if self.columns.is_empty() {
                return Err(tr("Add at least one column").into());
            }
            let mut seen = std::collections::HashSet::new();
            for c in &self.columns {
                let name = c.name.trim();
                if name.is_empty() {
                    return Err(tr("Every column needs a name").into());
                }
                if c.data_type.trim().is_empty() {
                    return Err(trf("Column “{name}” needs a type", &[("name", &name)]));
                }
                if !seen.insert(name.to_lowercase()) {
                    return Err(trf("Duplicate column “{name}”", &[("name", &name)]));
                }
            }
        }
        Ok(vec![self.sql()])
    }

    /// The server's choices arrived: the defaults are preselected so the user sees them.
    pub fn set_choices(&mut self, choices: crate::db::create_db::DatabaseChoices) {
        let o = &mut self.db_options;
        if o.charset.is_empty() {
            o.charset.clone_from(&choices.default_charset);
        }
        if o.collation.is_empty() {
            o.collation.clone_from(&choices.default_collation);
        }
        self.choices = Some(choices);
    }

    /// `schemas`: the database's schemas, to choose from (empty if not loaded yet).
    pub fn show(&mut self, ctx: &egui::Context, schemas: &[String]) -> Option<CreateResult> {
        let mut result = None;
        let p = palette(ctx.global_style().visuals.dark_mode);
        let frame = egui::Frame::popup(&ctx.global_style())
            .fill(p.popup_bg)
            .corner_radius(12)
            .inner_margin(Margin::same(20))
            .stroke(egui::Stroke::new(1.0, p.popup_border));
        let is_table = matches!(self.target, CreateTarget::Table { .. });
        egui::Modal::new(egui::Id::new("create_dialog")).frame(frame).show(ctx, |ui| {
            ui.set_width(if is_table { 640.0 } else { 480.0 });
            ui.horizontal(|ui| {
                let title = if is_table { tr("New Table") } else { tr("New Database") };
                ui.label(RichText::new(title).font(theme::bold(15.0)).color(p.text));
                if let CreateTarget::Table { db, .. } = &self.target {
                    ui.add_space(6.0);
                    ui.label(RichText::new(trf("in {db}", &[("db", db)])).color(p.muted));
                }
            });
            ui.add_space(14.0);
            ui.add_enabled_ui(!self.running, |ui| {
                self.form(ui, schemas);
                if is_table {
                    ui.add_space(12.0);
                    self.columns_editor(ui);
                }
            });
            ui.add_space(12.0);
            self.preview(ui);
            if let Some(e) = &self.error {
                ui.add_space(8.0);
                ui.add(egui::Label::new(RichText::new(e).monospace().color(p.error)).wrap());
            }
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                if theme::secondary_button(ui, tr("Copy SQL")).clicked() {
                    ui.ctx().copy_text(format!("{};", self.sql()));
                }
                if self.running {
                    ui.add_space(8.0);
                    ui.add(egui::Spinner::new().size(14.0).color(p.muted));
                    ui.label(RichText::new(tr("Creating…")).color(p.muted));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    ui.add_enabled_ui(!self.running, |ui| {
                        if theme::primary_button(ui, tr("Create")).clicked() {
                            match self.validate() {
                                Ok(statements) => result = Some(CreateResult::Create(statements)),
                                Err(e) => self.error = Some(e),
                            }
                        }
                        if theme::secondary_button(ui, tr("Cancel")).clicked() {
                            result = Some(CreateResult::Cancel);
                        }
                    });
                });
            });
        });
        if result.is_none() && !self.running && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            result = Some(CreateResult::Cancel);
        }
        result
    }

    fn form(&mut self, ui: &mut Ui, schemas: &[String]) {
        let driver = self.driver;
        egui::Grid::new("create_form").num_columns(2).spacing([14.0, 10.0]).min_col_width(80.0).show(ui, |ui| {
            ui.label(tr("Name:"));
            let name = ui.add(egui::TextEdit::singleline(&mut self.name).margin(vec2(8.0, 5.0)).desired_width(f32::INFINITY));
            if std::mem::take(&mut self.focus_name) {
                name.request_focus();
            }
            ui.end_row();
            if matches!(self.target, CreateTarget::Database) {
                database_options(ui, driver, &mut self.db_options, self.choices.as_ref());
            }
            if let CreateTarget::Table { schema, .. } = &mut self.target
                && !driver.flat_schema()
            {
                ui.label(tr("Schema:"));
                if schemas.is_empty() {
                    ui.add(egui::TextEdit::singleline(schema).margin(vec2(8.0, 5.0)).desired_width(220.0));
                } else {
                    egui::ComboBox::from_id_salt("create_schema").selected_text(schema.as_str()).width(220.0).show_ui(ui, |ui| {
                        for s in schemas {
                            ui.selectable_value(schema, s.clone(), s);
                        }
                    });
                }
                ui.end_row();
            }
        });
    }

    fn columns_editor(&mut self, ui: &mut Ui) {
        columns_editor(ui, "create", self.driver, &mut self.columns, |c| c, |c| c, 1);
    }

    fn preview(&self, ui: &mut Ui) {
        sql_preview(ui, "create", &format!("{};", self.sql()));
    }
}

/// The column list of "New Table" and "Modify Table": name, type (with a menu of usual types),
/// NOT NULL, primary key and default, one row per column, plus "Add Column". `column` gives
/// each row's column and `new_row` wraps a new one; at least `min_rows` rows are kept.
/// Returns the index of the row removed this frame.
pub(crate) fn columns_editor<T>(
    ui: &mut Ui,
    id: &str,
    driver: Driver,
    rows: &mut Vec<T>,
    column: impl Fn(&mut T) -> &mut NewColumn,
    new_row: impl FnOnce(NewColumn) -> T,
    min_rows: usize,
) -> Option<usize> {
    let p = theme::pal(ui);
    let types = common_types(driver);
    ui.label(RichText::new(tr("Columns")).font(theme::bold(13.0)));
    ui.add_space(6.0);
    let mut remove = None;
    let can_remove = rows.len() > min_rows;
    egui::ScrollArea::vertical().id_salt((id, "columns")).max_height(220.0).auto_shrink([false, true]).show(ui, |ui| {
        egui::Grid::new((id, "columns_grid")).num_columns(6).spacing([8.0, 6.0]).show(ui, |ui| {
            for h in [tr("Name"), tr("Type"), tr("Not null"), "PK", tr("Default"), ""] {
                ui.label(RichText::new(h).small().color(p.muted));
            }
            ui.end_row();
            for (i, row) in rows.iter_mut().enumerate() {
                let c = column(row);
                ui.add_sized([160.0, 26.0], egui::TextEdit::singleline(&mut c.name).id_salt((id, "col_name", i)).margin(vec2(6.0, 4.0)));
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    ui.add_sized(
                        [150.0, 26.0],
                        egui::TextEdit::singleline(&mut c.data_type)
                            .id_salt((id, "col_type", i))
                            .font(egui::TextStyle::Monospace)
                            .margin(vec2(6.0, 4.0)),
                    );
                    let pick = theme::icon_button(ui, Ico::ChevronDown, true, tr("Choose a type"));
                    egui::Popup::menu(&pick).show(|ui| {
                        for t in types {
                            if ui.button(RichText::new(*t).monospace()).clicked() {
                                c.data_type = (*t).to_string();
                                ui.close();
                            }
                        }
                    });
                });
                ui.add_enabled(!c.primary_key, egui::Checkbox::without_text(&mut c.not_null));
                if ui.add(egui::Checkbox::without_text(&mut c.primary_key)).changed() && c.primary_key {
                    c.not_null = true;
                }
                ui.add_sized(
                    [120.0, 26.0],
                    egui::TextEdit::singleline(&mut c.default)
                        .id_salt((id, "col_default", i))
                        .font(egui::TextStyle::Monospace)
                        .margin(vec2(6.0, 4.0)),
                );
                if theme::icon_button(ui, Ico::Minus, can_remove, tr("Remove column")).clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
    });
    if let Some(i) = remove {
        rows.remove(i);
    }
    ui.add_space(4.0);
    if theme::flat_button(ui, Some(Ico::Plus), tr("Add Column"), FontId::proportional(13.0), false, true).clicked() {
        let data_type = types.iter().find(|t| t.contains("varchar") || **t == "TEXT").or(types.first()).copied().unwrap_or_default();
        rows.push(new_row(NewColumn {
            name: String::new(),
            data_type: data_type.to_string(),
            not_null: false,
            primary_key: false,
            default: String::new(),
        }));
    }
    remove
}

/// Read-only, highlighted SQL preview.
pub(crate) fn sql_preview(ui: &mut Ui, id: &str, text: &str) {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    egui::Frame::NONE
        .fill(p.editor_bg)
        .stroke(egui::Stroke::new(1.0, p.control_border))
        .corner_radius(6)
        .inner_margin(Margin::same(8))
        .show(ui, |ui| {
            egui::ScrollArea::both().id_salt((id, "preview")).max_height(160.0).auto_shrink([false, true]).show(ui, |ui| {
                let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, _w: f32| {
                    let mut job = sql::highlight(buf.as_str(), dark, FontId::monospace(12.5));
                    job.wrap.max_width = f32::INFINITY;
                    ui.fonts_mut(|f| f.layout_job(job))
                };
                let mut t = text;
                ui.add(
                    egui::TextEdit::multiline(&mut t)
                        .id(egui::Id::new((id, "preview_sql")))
                        .code_editor()
                        .frame(egui::Frame::NONE)
                        .desired_width(f32::INFINITY)
                        .desired_rows(2)
                        .layouter(&mut layouter),
                );
            });
        });
}

/// "New Database" rows of the form grid, per engine.
fn database_options(ui: &mut Ui, driver: Driver, o: &mut crate::db::create_db::DatabaseOptions, choices: Option<&crate::db::create_db::DatabaseChoices>) {
    let p = theme::pal(ui);
    let text = |ui: &mut Ui, v: &mut String, hint: &str| {
        ui.add(egui::TextEdit::singleline(v).margin(vec2(8.0, 5.0)).desired_width(260.0).hint_text(hint));
    };
    let loading = |ui: &mut Ui| {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(12.0).color(p.muted));
            ui.label(RichText::new(tr("loading…")).color(p.muted));
        });
    };
    match driver {
        Driver::MySql | Driver::MariaDb => {
            ui.label(tr("Character set:"));
            match choices {
                None => loading(ui),
                Some(c) => {
                    let before = o.charset.clone();
                    let names: Vec<String> = c.charsets.iter().map(|(n, _)| n.clone()).collect();
                    searchable_combo(ui, "db_charset", &mut o.charset, &names, tr("Default"));
                    // A new character set takes its default collation.
                    if o.charset != before {
                        o.collation = c.charsets.iter().find(|(n, _)| *n == o.charset).map(|(_, d)| d.clone()).unwrap_or_default();
                    }
                }
            }
            ui.end_row();
            ui.label(tr("Collation:"));
            if let Some(c) = choices {
                let names: Vec<String> =
                    c.collations.iter().filter(|(_, cs)| o.charset.is_empty() || *cs == o.charset).map(|(n, _)| n.clone()).collect();
                searchable_combo(ui, "db_collation", &mut o.collation, &names, tr("Default"));
            }
            ui.end_row();
            if choices.is_some_and(|c| c.mariadb) {
                ui.label(tr("Comment:"));
                text(ui, &mut o.comment, "");
                ui.end_row();
            }
        }
        Driver::Postgres => {
            ui.label(tr("Owner:"));
            match choices {
                None => loading(ui),
                Some(c) => searchable_combo(ui, "db_owner", &mut o.owner, &c.roles, &c.current_role),
            }
            ui.end_row();
            ui.label(tr("Encoding:"));
            let encodings: Vec<String> = crate::db::create_db::PG_ENCODINGS.iter().map(|s| s.to_string()).collect();
            searchable_combo(ui, "db_encoding", &mut o.encoding, &encodings, tr("Default"));
            ui.end_row();
            ui.label(tr("Template:"));
            let templates = vec!["template1".to_string(), "template0".to_string()];
            searchable_combo(ui, "db_template", &mut o.template, &templates, tr("Default"));
            ui.end_row();
            ui.label(tr("Locale:"));
            text(ui, &mut o.locale, "en_US.UTF-8");
            ui.end_row();
            ui.label(tr("Connection limit:"));
            text(ui, &mut o.connection_limit, "-1");
            ui.end_row();
        }
        Driver::MsSql => {
            ui.label(tr("Collation:"));
            match choices {
                None => loading(ui),
                Some(c) => {
                    let names: Vec<String> = c.collations.iter().map(|(n, _)| n.clone()).collect();
                    searchable_combo(ui, "db_collation", &mut o.collation, &names, tr("Default"));
                }
            }
            ui.end_row();
        }
        Driver::Snowflake => {
            ui.label("");
            ui.checkbox(&mut o.transient, tr("Transient (no Fail-safe)"));
            ui.end_row();
            ui.label(tr("Data retention (days):"));
            text(ui, &mut o.retention_days, "1");
            ui.end_row();
            ui.label(tr("Comment:"));
            text(ui, &mut o.comment, "");
            ui.end_row();
        }
        _ => {}
    }
}

/// A menu of `options` with a filter box (collation lists run into the thousands); the empty
/// value is shown as `empty`.
fn searchable_combo(ui: &mut Ui, id: &str, value: &mut String, options: &[String], empty: &str) {
    let filter_id = egui::Id::new((id, "filter"));
    let shown = if value.is_empty() { empty.to_string() } else { value.clone() };
    egui::ComboBox::from_id_salt(id).selected_text(shown).width(260.0).height(320.0).show_ui(ui, |ui| {
        let mut filter: String = ui.data_mut(|d| d.get_temp(filter_id)).unwrap_or_default();
        ui.add(egui::TextEdit::singleline(&mut filter).hint_text(tr("Filter")).desired_width(240.0));
        ui.data_mut(|d| d.insert_temp(filter_id, filter.clone()));
        if ui.selectable_label(value.is_empty(), empty).clicked() {
            value.clear();
        }
        let f = filter.to_lowercase();
        for o in options.iter().filter(|o| f.is_empty() || o.to_lowercase().contains(&f)).take(500) {
            if ui.selectable_label(value == o, o).clicked() {
                value.clone_from(o);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str, not_null: bool, pk: bool, default: &str) -> NewColumn {
        NewColumn { name: name.into(), data_type: ty.into(), not_null, primary_key: pk, default: default.into() }
    }

    #[test]
    fn database_sql_quotes_when_needed() {
        assert_eq!(crate::db::create_db::create_database_sql(Driver::Postgres, " shop ", &Default::default(), false), "create database shop");
        assert_eq!(crate::db::create_db::create_database_sql(Driver::Postgres, "Shop", &Default::default(), false), "create database \"Shop\"");
        assert_eq!(crate::db::create_db::create_database_sql(Driver::MsSql, "My Shop", &Default::default(), false), "create database [My Shop]");
    }

    #[test]
    fn table_sql_per_dialect() {
        let cols = [col("id", "integer", false, true, ""), col("name", "varchar(255)", true, false, "'n/a'"), col("note", "text", false, false, "")];
        assert_eq!(
            create_table_sql(Driver::Postgres, "public", "items", &cols),
            "create table public.items\n(\n    id integer not null,\n    name varchar(255) not null default 'n/a',\n    note text,\n    primary key (id)\n)"
        );
        assert!(create_table_sql(Driver::MsSql, "dbo", "Order", &cols).starts_with("create table dbo.[Order]\n"));
        assert!(create_table_sql(Driver::Sqlite, "main", "items", &cols).starts_with("create table items\n"));
    }

    #[test]
    fn clickhouse_table_sql() {
        // Nullability goes in the type; MergeTree with the PK as ORDER BY (or `tuple()`).
        let cols = [col("id", "UInt64", false, true, ""), col("name", "String", true, false, "'n/a'"), col("note", "Nullable(String)", false, false, "")];
        assert_eq!(
            create_table_sql(Driver::ClickHouse, "default", "items", &cols),
            "create table `default`.items\n(\n    id UInt64,\n    name String default 'n/a',\n    note Nullable(String),\n    primary key (id)\n)\nengine = MergeTree\norder by (id)"
        );
        let cols = [col("x", "String", false, false, "")];
        assert!(create_table_sql(Driver::ClickHouse, "db", "t", &cols).ends_with("    x String null\n)\nengine = MergeTree\norder by (tuple())"));
    }

    #[test]
    fn composite_primary_key() {
        let cols = [col("a", "int", false, true, ""), col("b", "int", false, true, "")];
        assert!(create_table_sql(Driver::MsSql, "dbo", "t", &cols).ends_with("    primary key (a, b)\n)"));
    }

    #[test]
    fn validation() {
        let mut d = CreateDialog::table(1, Driver::Postgres, "db".into(), "public".into());
        assert!(d.validate().is_err(), "name required");
        d.name = "t".into();
        assert_eq!(d.validate().unwrap().len(), 1);
        d.columns.push(col("ID", "int", false, false, ""));
        assert!(d.validate().is_err(), "duplicate column");
        d.columns[1].name = "x".into();
        d.columns[1].data_type = " ".into();
        assert!(d.validate().is_err(), "missing type");
    }

    /// Writes previews of "New Database" to target/dialog-snapshots.
    #[test]
    #[ignore = "Requires a GPU; writes dialog previews to target/dialog-snapshots"]
    fn render_new_database() {
        use crate::db::create_db::DatabaseChoices;
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/dialog-snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        let my = DatabaseChoices {
            charsets: vec![("latin1".into(), "latin1_swedish_ci".into()), ("utf8mb4".into(), "utf8mb4_general_ci".into())],
            collations: vec![("utf8mb4_bin".into(), "utf8mb4".into()), ("utf8mb4_general_ci".into(), "utf8mb4".into())],
            default_charset: "utf8mb4".into(),
            default_collation: "utf8mb4_general_ci".into(),
            mariadb: true,
            ..Default::default()
        };
        let pg = DatabaseChoices { roles: vec!["app".into(), "postgres".into()], current_role: "postgres".into(), ..Default::default() };
        for (name, driver, choices) in [("newdb-mariadb", Driver::MariaDb, my), ("newdb-postgres", Driver::Postgres, pg)] {
            let mut d = CreateDialog::database(1, driver);
            d.name = "inventory".into();
            d.set_choices(choices);
            d.db_options.comment = "Stock".into();
            d.db_options.encoding = if driver == Driver::Postgres { "UTF8".into() } else { String::new() };
            let mut first = true;
            let mut h = egui_kittest::Harness::builder().with_size(vec2(760.0, 620.0)).build_ui_state(
                move |ui, d: &mut CreateDialog| {
                    if std::mem::take(&mut first) {
                        theme::install_fonts(ui.ctx());
                        theme::apply(ui.ctx(), false);
                        return;
                    }
                    d.show(ui.ctx(), &[]);
                },
                d,
            );
            h.run_steps(4);
            h.render().unwrap().save(dir.join(format!("{name}.png"))).unwrap();
        }
    }

}
