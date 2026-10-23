//! Structured server-side filters, with an explicit SQL text mode and Apply boundary.
use crate::db::{ColumnInfo, Driver};
use crate::edit::literal;
use crate::i18n::tr;
use crate::ui::{icons::Ico, theme};
use egui::Ui;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operator {
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    Contains,
    NotContains,
    Begins,
    NotBegins,
    Ends,
    NotEnds,
    Null,
    NotNull,
    Empty,
    NotEmpty,
    Between,
    NotBetween,
    In,
    NotIn,
}

impl Operator {
    const ALL: [Self; 20] = [
        Self::Equal,
        Self::NotEqual,
        Self::Less,
        Self::LessEqual,
        Self::Greater,
        Self::GreaterEqual,
        Self::Contains,
        Self::NotContains,
        Self::Begins,
        Self::NotBegins,
        Self::Ends,
        Self::NotEnds,
        Self::Null,
        Self::NotNull,
        Self::Empty,
        Self::NotEmpty,
        Self::Between,
        Self::NotBetween,
        Self::In,
        Self::NotIn,
    ];
    fn label(self) -> &'static str {
        match self {
            Self::Equal => "=",
            Self::NotEqual => "<>",
            Self::Less => "<",
            Self::LessEqual => "<=",
            Self::Greater => ">",
            Self::GreaterEqual => ">=",
            Self::Contains => tr("contains"),
            Self::NotContains => tr("does not contain"),
            Self::Begins => tr("begins with"),
            Self::NotBegins => tr("does not begin with"),
            Self::Ends => tr("ends with"),
            Self::NotEnds => tr("does not end with"),
            Self::Null => tr("is null"),
            Self::NotNull => tr("is not null"),
            Self::Empty => tr("is empty"),
            Self::NotEmpty => tr("is not empty"),
            Self::Between => tr("is between"),
            Self::NotBetween => tr("is not between"),
            Self::In => tr("is in list"),
            Self::NotIn => tr("is not in list"),
        }
    }
}

#[derive(Clone)]
pub struct Condition {
    pub enabled: bool,
    pub column: String,
    pub operator: Operator,
    pub value: String,
    pub second: String,
}

impl Condition {
    fn new(column: String) -> Self {
        Self {
            enabled: true,
            column,
            operator: Operator::Equal,
            value: String::new(),
            second: String::new(),
        }
    }
    fn sql(&self, driver: Driver, columns: &[ColumnInfo]) -> Result<Option<String>, &'static str> {
        if !self.enabled {
            return Ok(None);
        }
        if !columns.iter().any(|c| c.name == self.column) {
            return Err(tr("Select an existing column"));
        }
        let col = driver.quote_ident(&self.column);
        let value = |s: &str| literal(driver, &Some(s.into()));
        let v = value(&self.value);
        let predicate = match self.operator {
            Operator::Equal => format!("{col} = {v}"),
            Operator::NotEqual => format!("{col} <> {v}"),
            Operator::Less => format!("{col} < {v}"),
            Operator::LessEqual => format!("{col} <= {v}"),
            Operator::Greater => format!("{col} > {v}"),
            Operator::GreaterEqual => format!("{col} >= {v}"),
            Operator::Null => format!("{col} IS NULL"),
            Operator::NotNull => format!("{col} IS NOT NULL"),
            Operator::Empty if driver == Driver::Oracle => format!("{col} IS NULL"),
            Operator::Empty => format!("{col} = ''"),
            Operator::NotEmpty if driver == Driver::Oracle => format!("{col} IS NOT NULL"),
            Operator::NotEmpty => format!("({col} IS NOT NULL AND {col} <> '')"),
            Operator::Between | Operator::NotBetween => format!(
                "{col} {}BETWEEN {v} AND {}",
                if self.operator == Operator::NotBetween {
                    "NOT "
                } else {
                    ""
                },
                value(&self.second)
            ),
            Operator::In | Operator::NotIn => {
                let values: Vec<_> = self
                    .value
                    .lines()
                    .filter(|s| !s.is_empty())
                    .map(value)
                    .collect();
                if values.is_empty() {
                    return Err(tr("Enter one list value per line"));
                }
                format!(
                    "{col} {}IN ({})",
                    if self.operator == Operator::NotIn {
                        "NOT "
                    } else {
                        ""
                    },
                    values.join(", ")
                )
            }
            operator => {
                let mut text = self
                    .value
                    .replace('!', "!!")
                    .replace('%', "!%")
                    .replace('_', "!_");
                if driver == Driver::MsSql {
                    text = text.replace('[', "![");
                }
                let text = match operator {
                    Operator::Begins | Operator::NotBegins => format!("{text}%"),
                    Operator::Ends | Operator::NotEnds => format!("%{text}"),
                    _ => format!("%{text}%"),
                };
                let not = matches!(
                    operator,
                    Operator::NotContains | Operator::NotBegins | Operator::NotEnds
                );
                let text_col = match driver {
                    Driver::MsSql => format!("CONVERT(nvarchar(max), {col})"),
                    Driver::MySql | Driver::MariaDb => format!("CAST({col} AS CHAR)"),
                    Driver::Oracle => format!("CAST({col} AS VARCHAR2(4000))"),
                    _ => format!("CAST({col} AS TEXT)"),
                };
                format!(
                    "{text_col} {}LIKE {} ESCAPE '!'",
                    if not { "NOT " } else { "" },
                    value(&text)
                )
            }
        };
        Ok(Some(predicate))
    }
}

#[derive(Clone)]
pub enum Node {
    Condition(Condition),
    Group(Group),
}

#[derive(Clone, Default)]
pub struct Group {
    pub children: Vec<FilterRow>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Connector {
    #[default]
    And,
    Or,
}

impl Connector {
    fn keyword(self) -> &'static str {
        match self {
            Self::And => "AND",
            Self::Or => "OR",
        }
    }

    fn show(&mut self, ui: &mut Ui, has_next: bool) {
        if has_next {
            let response = ui.add_sized(
                egui::vec2(48.0, 22.0),
                egui::Button::new(self.keyword()).selected(*self == Self::Or),
            );
            if response
                .on_hover_text(tr(
                    "Joins the next enabled condition. Click to switch AND/OR.",
                ))
                .clicked()
            {
                *self = match self {
                    Self::And => Self::Or,
                    Self::Or => Self::And,
                };
            }
        } else {
            ui.allocate_space(egui::vec2(48.0, 22.0));
        }
    }
}

#[derive(Clone)]
pub struct FilterRow {
    pub node: Node,
    /// Connects this row to the next enabled sibling, using SQL's AND-before-OR precedence.
    pub connector: Connector,
}

impl From<Node> for FilterRow {
    fn from(node: Node) -> Self {
        Self {
            node,
            connector: Connector::And,
        }
    }
}

impl Group {
    fn sql(&self, driver: Driver, columns: &[ColumnInfo]) -> Result<Option<String>, &'static str> {
        let mut clauses = Vec::new();
        let mut previous = Connector::And;
        for row in &self.children {
            let sql = match &row.node {
                Node::Condition(c) => c.sql(driver, columns)?,
                Node::Group(g) => g.sql(driver, columns)?,
            };
            if let Some(sql) = sql {
                if !clauses.is_empty() {
                    clauses.push(previous.keyword().into());
                }
                clauses.push(sql);
                previous = row.connector;
            }
        }
        Ok((!clauses.is_empty()).then(|| format!("({})", clauses.join(" "))))
    }

    fn show(&mut self, ui: &mut Ui, columns: &[ColumnInfo], depth: usize) {
        ui.horizontal(|ui| {
            if theme::add_button(ui, Ico::Plus, tr("Add condition")).clicked() {
                self.children.push(
                    Node::Condition(Condition::new(
                        columns.first().map(|c| c.name.clone()).unwrap_or_default(),
                    ))
                    .into(),
                );
            }
            if ui
                .add_enabled_ui(depth < 8, |ui| {
                    theme::add_button(ui, Ico::Folder, tr("Add group"))
                })
                .inner
                .clicked()
            {
                self.children.push(Node::Group(Group::default()).into());
            }
        });
        let mut remove = None;
        let len = self.children.len();
        for (index, row) in self.children.iter_mut().enumerate() {
            ui.push_id(index, |ui| match &mut row.node {
                Node::Group(group) => {
                    ui.horizontal(|ui| {
                        ui.strong("(");
                        ui.label(tr("Condition group"));
                    });
                    ui.indent("nested", |ui| group.show(ui, columns, depth + 1));
                    ui.horizontal(|ui| {
                        ui.strong(")");
                        row.connector.show(ui, index + 1 < len);
                        if ui.small_button("×").clicked() {
                            remove = Some(index);
                        }
                    });
                }
                Node::Condition(c) => {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut c.enabled, "");
                        column_picker(ui, "column", &mut c.column, columns);
                        egui::ComboBox::from_id_salt("operator")
                            .width(130.0)
                            .selected_text(c.operator.label())
                            .show_ui(ui, |ui| {
                                for op in Operator::ALL {
                                    ui.selectable_value(&mut c.operator, op, op.label());
                                }
                            });
                        if !matches!(
                            c.operator,
                            Operator::Null
                                | Operator::NotNull
                                | Operator::Empty
                                | Operator::NotEmpty
                        ) {
                            if matches!(c.operator, Operator::In | Operator::NotIn) {
                                ui.add(
                                    egui::TextEdit::multiline(&mut c.value)
                                        .desired_rows(2)
                                        .desired_width(220.0)
                                        .hint_text(tr("Enter one list value per line")),
                                );
                            } else {
                                ui.add(
                                    egui::TextEdit::singleline(&mut c.value)
                                        .desired_width(160.0)
                                        .hint_text(tr("Value")),
                                );
                            }
                        } else {
                            ui.allocate_space(egui::vec2(160.0, 22.0));
                        }
                        if matches!(c.operator, Operator::Between | Operator::NotBetween) {
                            ui.label(tr("and"));
                            ui.add(egui::TextEdit::singleline(&mut c.second).desired_width(140.0));
                        }
                        row.connector.show(ui, index + 1 < len);
                        if ui.small_button("×").clicked() {
                            remove = Some(index);
                        }
                    });
                }
            });
        }
        if let Some(index) = remove {
            self.children.remove(index);
        }
    }
}

#[derive(Clone)]
pub struct SortRule {
    pub enabled: bool,
    pub column: String,
    pub descending: bool,
}

#[derive(Default)]
pub struct FilterSort {
    pub text_mode: bool,
    pub filter: Group,
    pub sort: Vec<SortRule>,
    pub where_text: String,
    pub order_text: String,
    pub pending: Option<(String, String)>,
    error: Option<String>,
}

impl FilterSort {
    pub fn sql(
        &self,
        driver: Driver,
        columns: &[ColumnInfo],
    ) -> Result<(String, String), &'static str> {
        if self.text_mode {
            for (keyword, text) in [("WHERE", &self.where_text), ("ORDER BY", &self.order_text)] {
                if !text.trim().is_empty()
                    && crate::sql::split_statements(
                        &format!("SELECT * FROM dc_filter {keyword} {text}"),
                        driver,
                    )
                    .len()
                        != 1
                {
                    return Err(tr("Filter text must be a single SQL expression"));
                }
            }
            return Ok((self.where_text.clone(), self.order_text.clone()));
        }
        self.builder_sql(driver, columns)
    }

    fn builder_sql(
        &self,
        driver: Driver,
        columns: &[ColumnInfo],
    ) -> Result<(String, String), &'static str> {
        let mut sorts = Vec::new();
        for rule in self.sort.iter().filter(|r| r.enabled) {
            if !columns.iter().any(|c| c.name == rule.column) {
                return Err(tr("Select an existing column"));
            }
            sorts.push(format!(
                "{} {}",
                driver.quote_ident(&rule.column),
                if rule.descending { "DESC" } else { "ASC" }
            ));
        }
        Ok((
            self.filter.sql(driver, columns)?.unwrap_or_default(),
            sorts.join(", "),
        ))
    }

    pub fn show(&mut self, ui: &mut Ui, driver: Driver, columns: &[ColumnInfo]) -> bool {
        let mut apply = false;
        egui::Frame::NONE
            .inner_margin(egui::Margin::symmetric(8, 6))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(6.0, 3.0);
                ui.spacing_mut().interact_size.y = 22.0;
                ui.spacing_mut().button_padding = egui::vec2(6.0, 2.0);
                egui::ScrollArea::vertical()
                    .id_salt("filter_sort_builder")
                    .max_height((ui.available_height() * 0.5).clamp(200.0, 440.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.strong(tr("Filter"));
                            if ui
                                .selectable_label(!self.text_mode, tr("Builder"))
                                .clicked()
                            {
                                if self.text_mode
                                    && self.builder_sql(driver, columns).is_ok_and(|sql| {
                                        sql != (self.where_text.clone(), self.order_text.clone())
                                    })
                                {
                                    self.error = Some(
                                        tr("Clear custom SQL before switching to Builder").into(),
                                    );
                                } else {
                                    self.text_mode = false;
                                }
                            }
                            if ui.selectable_label(self.text_mode, tr("Text")).clicked() {
                                if !self.text_mode
                                    && let Ok((w, o)) = self.sql(driver, columns)
                                {
                                    self.where_text = w;
                                    self.order_text = o;
                                }
                                self.text_mode = true;
                            }
                        });
                        if self.text_mode {
                            ui.add(
                                egui::TextEdit::multiline(&mut self.where_text)
                                    .desired_width(f32::INFINITY)
                                    .desired_rows(2)
                                    .font(egui::TextStyle::Monospace)
                                    .hint_text("WHERE"),
                            );
                        } else {
                            egui::ScrollArea::horizontal()
                                .id_salt("filter_condition_rows")
                                .auto_shrink([false, true])
                                .show(ui, |ui| self.filter.show(ui, columns, 0));
                        }
                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.strong(tr("Sort By"));
                            if !self.text_mode
                                && theme::add_button(ui, Ico::Plus, tr("Add sort column")).clicked()
                            {
                                self.sort.push(SortRule {
                                    enabled: true,
                                    column: columns
                                        .first()
                                        .map(|c| c.name.clone())
                                        .unwrap_or_default(),
                                    descending: false,
                                });
                            }
                        });
                        if self.text_mode {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.order_text)
                                    .desired_width(f32::INFINITY)
                                    .hint_text("ORDER BY"),
                            );
                        } else {
                            let mut remove = None;
                            let mut swap = None;
                            let len = self.sort.len();
                            for (index, rule) in self.sort.iter_mut().enumerate() {
                                ui.push_id(("sort", index), |ui| {
                                    ui.horizontal(|ui| {
                                        ui.checkbox(&mut rule.enabled, "");
                                        column_picker(ui, "sort_column", &mut rule.column, columns);
                                        egui::ComboBox::from_id_salt("sort_direction")
                                            .width(110.0)
                                            .selected_text(if rule.descending {
                                                tr("Descending")
                                            } else {
                                                tr("Ascending")
                                            })
                                            .show_ui(ui, |ui| {
                                                ui.selectable_value(
                                                    &mut rule.descending,
                                                    false,
                                                    tr("Ascending"),
                                                );
                                                ui.selectable_value(
                                                    &mut rule.descending,
                                                    true,
                                                    tr("Descending"),
                                                );
                                            });
                                        if ui
                                            .add_enabled(index > 0, egui::Button::new("↑"))
                                            .clicked()
                                        {
                                            swap = Some((index, index - 1));
                                        }
                                        if ui
                                            .add_enabled(index + 1 < len, egui::Button::new("↓"))
                                            .clicked()
                                        {
                                            swap = Some((index, index + 1));
                                        }
                                        if ui.small_button("×").clicked() {
                                            remove = Some(index);
                                        }
                                    });
                                });
                            }
                            if let Some(index) = remove {
                                self.sort.remove(index);
                            } else if let Some((a, b)) = swap {
                                self.sort.swap(a, b);
                            }
                        }
                        ui.horizontal(|ui| {
                            if theme::tool_button(ui, Ico::Submit, tr("Apply"), false).clicked() {
                                match self.sql(driver, columns) {
                                    Ok(sql) => {
                                        self.pending = Some(sql);
                                        self.error = None;
                                        apply = true;
                                    }
                                    Err(error) => self.error = Some(error.into()),
                                }
                            }
                            if theme::tool_button(ui, Ico::Revert, tr("Clear"), false).clicked() {
                                self.filter = Group::default();
                                self.sort.clear();
                                self.where_text.clear();
                                self.order_text.clear();
                                self.error = None;
                            }
                            if let Some(error) = &self.error {
                                ui.colored_label(ui.visuals().error_fg_color, error);
                            }
                        });
                    });
            });
        apply
    }
}

fn column_picker(ui: &mut Ui, id: &str, column: &mut String, columns: &[ColumnInfo]) {
    egui::ComboBox::from_id_salt(id)
        .selected_text(column.as_str())
        .width(150.0)
        .show_ui(ui, |ui| {
            for c in columns {
                ui.selectable_value(column, c.name.clone(), &c.name);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    fn columns() -> Vec<ColumnInfo> {
        vec![ColumnInfo {
            name: "odd]name".into(),
            data_type: "text".into(),
            nullable: true,
            primary_key: false,
        }]
    }
    #[test]
    fn builder_executes_literal_filters_and_multi_column_order_in_sqlite() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE sample (id INTEGER, value TEXT); INSERT INTO sample VALUES (1, '50%'), (2, '50x'), (3, '50%');").unwrap();
        let columns = vec![
            ColumnInfo {
                name: "id".into(),
                data_type: "integer".into(),
                nullable: false,
                primary_key: true,
            },
            ColumnInfo {
                name: "value".into(),
                data_type: "text".into(),
                nullable: true,
                primary_key: false,
            },
        ];
        let mut c = Condition::new("value".into());
        c.operator = Operator::Contains;
        c.value = "50%".into();
        let builder = FilterSort {
            filter: Group {
                children: vec![Node::Condition(c).into()],
            },
            sort: vec![
                SortRule {
                    enabled: true,
                    column: "value".into(),
                    descending: false,
                },
                SortRule {
                    enabled: true,
                    column: "id".into(),
                    descending: true,
                },
            ],
            ..Default::default()
        };
        let (w, o) = builder.sql(Driver::Sqlite, &columns).unwrap();
        let rows: Vec<i64> = db
            .prepare(&format!("SELECT id FROM sample WHERE {w} ORDER BY {o}"))
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(rows, vec![3, 1]);
        let raw = FilterSort {
            text_mode: true,
            where_text: "1=1; DROP TABLE sample".into(),
            ..Default::default()
        };
        assert!(raw.sql(Driver::Sqlite, &columns).is_err());
    }

    #[test]
    fn nested_filters_escape_values_and_preserve_disabled_nodes_and_sort_order() {
        let mut c = Condition::new("odd]name".into());
        c.operator = Operator::Contains;
        c.value = "O'Reilly_50%[".into();
        let filter = FilterSort {
            filter: Group {
                children: vec![
                    Node::Condition(c.clone()).into(),
                    Node::Group(Group {
                        children: vec![
                            FilterRow {
                                connector: Connector::Or,
                                node: Node::Condition(Condition {
                                    operator: Operator::Null,
                                    ..c.clone()
                                }),
                            },
                            Node::Condition(Condition {
                                enabled: false,
                                ..c
                            })
                            .into(),
                        ],
                    })
                    .into(),
                ],
            },
            sort: vec![SortRule {
                enabled: true,
                column: "odd]name".into(),
                descending: true,
            }],
            ..Default::default()
        };
        let (w, o) = filter.sql(Driver::MsSql, &columns()).unwrap();
        assert_eq!(
            w,
            "(CONVERT(nvarchar(max), [odd]]name]) LIKE N'%O''Reilly!_50!%![%' ESCAPE '!' AND ([odd]]name] IS NULL))"
        );
        assert_eq!(o, "[odd]]name] DESC");
    }
    fn flag(name: &str, connector: Connector) -> FilterRow {
        FilterRow {
            node: Node::Condition(Condition {
                value: "1".into(),
                ..Condition::new(name.into())
            }),
            connector,
        }
    }

    #[test]
    fn mixed_row_connectors_obey_sql_precedence_and_explicit_groups() {
        let columns: Vec<_> = ["a", "b", "c", "d", "e"]
            .into_iter()
            .map(|name| ColumnInfo {
                name: name.into(),
                data_type: "integer".into(),
                nullable: true,
                primary_key: false,
            })
            .collect();
        let flat = Group {
            children: vec![
                flag("a", Connector::And),
                flag("b", Connector::And),
                flag("c", Connector::Or),
                flag("d", Connector::And),
                flag("e", Connector::Or),
            ],
        };
        let grouped = Group {
            children: vec![
                FilterRow {
                    connector: Connector::And,
                    node: Node::Group(Group {
                        children: vec![flag("a", Connector::Or), flag("b", Connector::And)],
                    }),
                },
                flag("c", Connector::And),
            ],
        };
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE flags (a INTEGER, b INTEGER, c INTEGER, d INTEGER, e INTEGER)",
        )
        .unwrap();
        for bits in 0..32 {
            let values: Vec<_> = (0..5).map(|index| (bits >> index) & 1).collect();
            db.execute("DELETE FROM flags", []).unwrap();
            db.execute(
                "INSERT INTO flags VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params_from_iter(&values),
            )
            .unwrap();
            for (group, expected) in [
                (
                    &flat,
                    (values[0] == 1 && values[1] == 1 && values[2] == 1)
                        || (values[3] == 1 && values[4] == 1),
                ),
                (
                    &grouped,
                    (values[0] == 1 || values[1] == 1) && values[2] == 1,
                ),
            ] {
                let predicate = group.sql(Driver::Sqlite, &columns).unwrap().unwrap();
                let actual: bool = db
                    .query_row(
                        &format!("SELECT EXISTS(SELECT 1 FROM flags WHERE {predicate})"),
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(actual, expected, "{predicate}, input {values:?}");
            }
        }
        for driver in Driver::ALL.into_iter().filter(|d| !d.is_nosql()) {
            let sql = flat.sql(driver, &columns).unwrap().unwrap();
            assert!(sql.contains(" AND ") && sql.contains(" OR "));
            assert!(!sql.ends_with("OR)"), "last connector must not be emitted");
        }
    }

    #[test]
    fn disabled_rows_and_empty_groups_do_not_supply_connectors() {
        let columns: Vec<_> = ["a", "b", "c"]
            .into_iter()
            .map(|name| ColumnInfo {
                name: name.into(),
                data_type: "integer".into(),
                nullable: true,
                primary_key: false,
            })
            .collect();
        let mut disabled = flag("b", Connector::And);
        if let Node::Condition(condition) = &mut disabled.node {
            condition.enabled = false;
        }
        let mut group = Group {
            children: vec![
                flag("a", Connector::Or),
                disabled,
                FilterRow {
                    node: Node::Group(Group::default()),
                    connector: Connector::And,
                },
                flag("c", Connector::Or),
            ],
        };
        assert_eq!(
            group.sql(Driver::Sqlite, &columns).unwrap().unwrap(),
            "(\"a\" = '1' OR \"c\" = '1')"
        );
        group.children.remove(0);
        assert_eq!(
            group.sql(Driver::Sqlite, &columns).unwrap().unwrap(),
            "(\"c\" = '1')"
        );
        group.children.pop();
        assert_eq!(group.sql(Driver::Sqlite, &columns).unwrap(), None);
    }

    #[test]
    fn inline_logic_stays_before_remove_and_applies_only_on_request() {
        use egui_kittest::kittest::Queryable;
        let mut first = true;
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1000.0, 500.0))
            .build_ui_state(
                move |ui, state: &mut FilterSort| {
                    if std::mem::take(&mut first) {
                        theme::install_fonts(ui.ctx());
                        return;
                    }
                    state.show(ui, Driver::Sqlite, &columns());
                },
                FilterSort {
                    filter: Group {
                        children: (0..5).map(|_| flag("odd]name", Connector::And)).collect(),
                    },
                    ..Default::default()
                },
            );
        h.run_steps(3);
        assert!(h.query_by_label("All (AND)").is_none());
        assert!(h.query_by_label("Any (OR)").is_none());
        let connector = h.get_all_by_label("AND").nth(2).unwrap();
        let remove = h.get_all_by_label("×").nth(2).unwrap();
        assert!(connector.rect().right() <= remove.rect().left());
        assert!((connector.rect().center().y - remove.rect().center().y).abs() < 2.0);
        connector.click();
        h.run_steps(3);
        assert_eq!(h.state().filter.children[2].connector, Connector::Or);
        assert!(h.state().pending.is_none());
        h.get_by_label("Apply").click();
        h.run_steps(3);
        let expected = "(\"odd]name\" = '1' AND \"odd]name\" = '1' AND \"odd]name\" = '1' OR \"odd]name\" = '1' AND \"odd]name\" = '1')";
        assert_eq!(h.state().pending.as_ref().unwrap().0, expected);
        h.get_by_label("Text").click();
        h.run_steps(3);
        assert_eq!(h.state().where_text, expected);
    }

    #[test]
    fn lists_are_literals_and_unknown_columns_are_rejected() {
        let mut c = Condition::new("odd]name".into());
        c.operator = Operator::In;
        c.value = "a\nx'); DROP TABLE x; --".into();
        let sql = c.sql(Driver::Sqlite, &columns()).unwrap().unwrap();
        assert_eq!(sql, "\"odd]name\" IN ('a', 'x''); DROP TABLE x; --')");
        c.column = "missing".into();
        assert!(c.sql(Driver::Sqlite, &columns()).is_err());
    }
}
