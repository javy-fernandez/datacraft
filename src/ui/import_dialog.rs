//! "Import Data from File" (DataGrip): loads a CSV/TSV file into an existing table, mapping
//! file columns to table columns, with a preview of the rows and of the generated SQL.

use crate::db::{Cell, ColumnInfo, Driver, Session};
use crate::edit::literal;
use crate::i18n::{tr, trf};
use crate::sql;
use crate::ui::quote_ident;
use crate::ui::theme::{self, palette};
use crate::worker::ConnId;
use egui::{FontId, Margin, RichText, vec2};

/// Rows per multi-row INSERT (SQL Server allows at most 1000).
const BATCH: usize = 100;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Delimiter {
    Comma,
    Semicolon,
    Tab,
    Pipe,
}

impl Delimiter {
    const ALL: [Delimiter; 4] = [
        Delimiter::Comma,
        Delimiter::Semicolon,
        Delimiter::Tab,
        Delimiter::Pipe,
    ];

    fn char(self) -> char {
        match self {
            Delimiter::Comma => ',',
            Delimiter::Semicolon => ';',
            Delimiter::Tab => '\t',
            Delimiter::Pipe => '|',
        }
    }

    fn label(self) -> &'static str {
        match self {
            Delimiter::Comma => tr(", (comma)"),
            Delimiter::Semicolon => tr("; (semicolon)"),
            Delimiter::Tab => tr("Tab"),
            Delimiter::Pipe => tr("| (pipe)"),
        }
    }

    /// The most frequent candidate in the first line.
    fn guess(text: &str) -> Delimiter {
        let first = text.lines().next().unwrap_or("");
        Delimiter::ALL
            .into_iter()
            .max_by_key(|d| first.matches(d.char()).count())
            .unwrap_or(Delimiter::Comma)
    }
}

/// RFC 4180 parser: quoted fields (with `""` escapes and line breaks inside), CRLF or LF.
pub fn parse_delimited(text: &str, delim: char) -> Vec<Vec<String>> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    // true once the current row has any content (avoids an empty row at the end).
    let mut started = false;
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' if field.is_empty() => {
                quoted = true;
                started = true;
            }
            c if c == delim => {
                row.push(std::mem::take(&mut field));
                started = true;
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' | '\r' => {
                if started || !field.is_empty() {
                    row.push(std::mem::take(&mut field));
                    rows.push(std::mem::take(&mut row));
                }
                started = false;
            }
            c => {
                field.push(c);
                started = true;
            }
        }
    }
    if started || !field.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

pub struct ImportDialog {
    pub conn: ConnId,
    pub driver: Driver,
    pub db: String,
    pub schema: String,
    pub table: String,
    columns: Vec<ColumnInfo>,
    path: String,
    text: String,
    delimiter: Delimiter,
    header: bool,
    empty_as_null: bool,
    /// For each table column, the file column it takes its value from (`None` = skip).
    mapping: Vec<Option<usize>>,
    rows: Vec<Vec<String>>,
    pub running: bool,
    pub error: Option<String>,
    /// Rows sent in the last import (for the final message).
    pub imported: usize,
}

pub enum ImportResult {
    Import(Vec<String>),
    Cancel,
}

impl ImportDialog {
    pub fn new(
        conn: ConnId,
        driver: Driver,
        db: String,
        schema: String,
        table: String,
        columns: Vec<ColumnInfo>,
    ) -> Self {
        let mapping = vec![None; columns.len()];
        Self {
            conn,
            driver,
            db,
            schema,
            table,
            columns,
            path: String::new(),
            text: String::new(),
            delimiter: Delimiter::Comma,
            header: true,
            empty_as_null: true,
            mapping,
            rows: Vec::new(),
            running: false,
            error: None,
            imported: 0,
        }
    }

    /// Loads a file (UTF-8; invalid sequences are replaced) and guesses the delimiter.
    pub fn load(&mut self, path: &std::path::Path) {
        match std::fs::read(path) {
            Ok(bytes) => {
                self.text = String::from_utf8_lossy(&bytes).into_owned();
                self.path = path.display().to_string();
                self.delimiter = match path.extension().and_then(|e| e.to_str()) {
                    Some(e) if e.eq_ignore_ascii_case("tsv") || e.eq_ignore_ascii_case("tab") => {
                        Delimiter::Tab
                    }
                    _ => Delimiter::guess(&self.text),
                };
                self.error = None;
                self.reparse(true);
            }
            Err(e) => {
                self.error = Some(trf(
                    "Could not open {path}: {error}",
                    &[("path", &path.display()), ("error", &e)],
                ))
            }
        }
    }

    fn reparse(&mut self, remap: bool) {
        self.rows = parse_delimited(&self.text, self.delimiter.char());
        if remap {
            self.auto_map();
        }
    }

    fn file_headers(&self) -> Vec<String> {
        let n = self.rows.iter().map(Vec::len).max().unwrap_or(0);
        match (self.header, self.rows.first()) {
            (true, Some(first)) => (0..n)
                .map(|i| {
                    first
                        .get(i)
                        .cloned()
                        .unwrap_or_else(|| format!("#{}", i + 1))
                })
                .collect(),
            _ => (0..n).map(|i| format!("#{}", i + 1)).collect(),
        }
    }

    fn data_rows(&self) -> &[Vec<String>] {
        if self.header && !self.rows.is_empty() {
            &self.rows[1..]
        } else {
            &self.rows
        }
    }

    /// By header name (case-insensitive); without header (or no match at all), by position.
    fn auto_map(&mut self) {
        let headers = self.file_headers();
        let by_name: Vec<Option<usize>> = self
            .columns
            .iter()
            .map(|c| {
                headers
                    .iter()
                    .position(|h| h.trim().eq_ignore_ascii_case(&c.name))
            })
            .collect();
        self.mapping = if self.header && by_name.iter().any(Option::is_some) {
            by_name
        } else {
            (0..self.columns.len())
                .map(|i| (i < headers.len()).then_some(i))
                .collect()
        };
    }

    /// Marks the dialog as running and returns the statements to send.
    pub fn start(&mut self) -> Vec<String> {
        self.running = true;
        self.error = None;
        self.imported = self.data_rows().len();
        self.statements()
    }

    fn statements(&self) -> Vec<String> {
        insert_statements(
            self.driver,
            &Session::quote_table(self.driver, &self.schema, &self.table),
            &self.columns,
            &self.mapping,
            self.data_rows(),
            self.empty_as_null,
        )
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Option<ImportResult> {
        let mut result = None;
        let dark = ctx.global_style().visuals.dark_mode;
        let p = palette(dark);
        let frame = egui::Frame::popup(&ctx.global_style())
            .fill(p.popup_bg)
            .corner_radius(12)
            .inner_margin(Margin::same(20))
            .stroke(egui::Stroke::new(1.0, p.popup_border));
        egui::Modal::new(egui::Id::new("import_dialog"))
            .frame(frame)
            .show(ctx, |ui| {
                ui.set_width(640.0);
                ui.label(
                    RichText::new(tr("Import Data from File"))
                        .font(theme::bold(15.0))
                        .color(p.text),
                );
                ui.label(
                    RichText::new(trf("into {table}", &[("table", &self.table)])).color(p.muted),
                );
                ui.add_space(10.0);
                ui.add_enabled_ui(!self.running, |ui| self.options(ui));
                if !self.rows.is_empty() {
                    ui.add_space(10.0);
                    ui.add_enabled_ui(!self.running, |ui| self.mapping_ui(ui));
                    ui.add_space(10.0);
                    let stmts = self.statements();
                    let first = stmts.first().cloned().unwrap_or_default();
                    let preview: String = first.lines().take(8).collect::<Vec<_>>().join("\n");
                    let more = if first.lines().count() > 8 {
                        "\n…"
                    } else {
                        ""
                    };
                    egui::Frame::NONE
                        .fill(p.editor_bg)
                        .stroke(egui::Stroke::new(1.0, p.control_border))
                        .corner_radius(6)
                        .inner_margin(Margin::same(8))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            let mut job = sql::highlight(
                                &format!("{preview}{more}"),
                                dark,
                                FontId::monospace(12.0),
                            );
                            job.wrap.max_width = ui.available_width();
                            ui.label(job);
                        });
                }
                if let Some(e) = &self.error {
                    ui.add_space(8.0);
                    ui.add(egui::Label::new(RichText::new(e).monospace().color(p.error)).wrap());
                }
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    let n = self.data_rows().len();
                    if self.running {
                        ui.add(egui::Spinner::new().size(14.0).color(p.muted));
                        ui.label(RichText::new(tr("Importing…")).color(p.muted));
                    } else if !self.rows.is_empty() {
                        ui.label(
                            RichText::new(trf("{n} rows to import", &[("n", &n)])).color(p.muted),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        let mapped = self.mapping.iter().any(Option::is_some);
                        ui.add_enabled_ui(!self.running && n > 0 && mapped, |ui| {
                            if theme::primary_button(ui, tr("Import")).clicked() {
                                result = Some(ImportResult::Import(self.start()));
                            }
                        });
                        ui.add_enabled_ui(!self.running, |ui| {
                            if theme::secondary_button(ui, tr("Cancel")).clicked() {
                                result = Some(ImportResult::Cancel);
                            }
                        });
                    });
                });
            });
        if result.is_none() && !self.running && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            result = Some(ImportResult::Cancel);
        }
        result
    }

    fn options(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("import_options")
            .num_columns(2)
            .spacing([14.0, 10.0])
            .min_col_width(80.0)
            .show(ui, |ui| {
                ui.label(tr("File:"));
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.path)
                            .margin(vec2(8.0, 5.0))
                            .desired_width(420.0)
                            .interactive(false),
                    );
                    if ui.button(tr("Browse…")).clicked()
                        && let Some(path) = rfd::FileDialog::new()
                            .set_title(tr("Import Data from File"))
                            .add_filter("CSV / TSV", &["csv", "tsv", "txt", "tab"])
                            .add_filter(tr("All files"), &["*"])
                            .pick_file()
                    {
                        self.load(&path);
                    }
                });
                ui.end_row();
                ui.label(tr("Separator:"));
                let mut delim = self.delimiter;
                egui::ComboBox::from_id_salt("import_delim")
                    .selected_text(delim.label())
                    .width(160.0)
                    .show_ui(ui, |ui| {
                        for d in Delimiter::ALL {
                            ui.selectable_value(&mut delim, d, d.label());
                        }
                    });
                if delim != self.delimiter {
                    self.delimiter = delim;
                    self.reparse(true);
                }
                ui.end_row();
                ui.label("");
                ui.horizontal(|ui| {
                    if ui
                        .checkbox(&mut self.header, tr("First row is header"))
                        .changed()
                    {
                        self.auto_map();
                    }
                    ui.checkbox(&mut self.empty_as_null, tr("Empty values as NULL"));
                });
                ui.end_row();
            });
    }

    fn mapping_ui(&mut self, ui: &mut egui::Ui) {
        let p = theme::pal(ui);
        let headers = self.file_headers();
        let sample = self.data_rows().first().cloned().unwrap_or_default();
        ui.label(RichText::new(tr("Columns")).font(theme::bold(13.0)));
        ui.add_space(6.0);
        egui::ScrollArea::vertical()
            .id_salt("import_map")
            .max_height(200.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                egui::Grid::new("import_map_grid")
                    .num_columns(3)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        for h in [tr("Table column"), tr("File column"), tr("First value")] {
                            ui.label(RichText::new(h).small().color(p.muted));
                        }
                        ui.end_row();
                        for (i, c) in self.columns.iter().enumerate() {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(&c.name).color(p.text));
                                ui.label(
                                    RichText::new(c.data_type.to_lowercase())
                                        .small()
                                        .color(p.muted),
                                );
                            });
                            let skip = tr("(skip)").to_string();
                            let current = self.mapping[i]
                                .and_then(|j| headers.get(j))
                                .cloned()
                                .unwrap_or_else(|| skip.clone());
                            egui::ComboBox::from_id_salt(("import_col", i))
                                .selected_text(current)
                                .width(180.0)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut self.mapping[i], None, skip.as_str());
                                    for (j, h) in headers.iter().enumerate() {
                                        ui.selectable_value(&mut self.mapping[i], Some(j), h);
                                    }
                                });
                            let value = self.mapping[i]
                                .and_then(|j| sample.get(j))
                                .cloned()
                                .unwrap_or_default();
                            ui.add(
                                egui::Label::new(RichText::new(value).monospace().color(p.muted))
                                    .truncate(),
                            );
                            ui.end_row();
                        }
                    });
            });
    }
}

/// INSERT statements for the mapped columns, `BATCH` rows each (Oracle: one row each, it has
/// no multi-row VALUES). Short rows give NULL for the missing fields.
pub fn insert_statements(
    driver: Driver,
    table: &str,
    columns: &[ColumnInfo],
    mapping: &[Option<usize>],
    rows: &[Vec<String>],
    empty_as_null: bool,
) -> Vec<String> {
    let used: Vec<(usize, usize)> = mapping
        .iter()
        .enumerate()
        .filter_map(|(c, m)| m.map(|j| (c, j)))
        .collect();
    if used.is_empty() {
        return Vec::new();
    }
    let cols = used
        .iter()
        .map(|&(c, _)| quote_ident(driver, &columns[c].name))
        .collect::<Vec<_>>()
        .join(", ");
    let tuple = |row: &Vec<String>| {
        let vals: Vec<String> = used
            .iter()
            .map(|&(_, j)| {
                let v: Cell = match row.get(j) {
                    Some(s) if !(empty_as_null && s.is_empty()) => Some(s.clone()),
                    Some(_) | None => None,
                };
                literal(driver, &v)
            })
            .collect();
        format!("({})", vals.join(", "))
    };
    let batch = if driver == Driver::Oracle { 1 } else { BATCH };
    rows.chunks(batch)
        .map(|chunk| {
            let values = chunk
                .iter()
                .map(tuple)
                .collect::<Vec<_>>()
                .join(",\n       ");
            format!("INSERT INTO {table} ({cols})\nVALUES {values}")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            data_type: "text".into(),
            nullable: true,
            primary_key: false,
        }
    }

    #[test]
    fn parses_quotes_and_line_breaks() {
        let rows = parse_delimited(
            "\u{feff}id,name\r\n1,\"a, \"\"b\"\"\"\n2,\"x\ny\"\n3,\n",
            ',',
        );
        assert_eq!(
            rows,
            vec![
                vec!["id".to_string(), "name".into()],
                vec!["1".into(), "a, \"b\"".into()],
                vec!["2".into(), "x\ny".into()],
                vec!["3".into(), "".into()],
            ]
        );
        assert_eq!(
            parse_delimited("a\tb", '\t'),
            vec![vec!["a".to_string(), "b".into()]]
        );
        assert!(parse_delimited("", ',').is_empty());
    }

    #[test]
    fn guesses_delimiter() {
        assert_eq!(Delimiter::guess("a;b;c\n1;2;3"), Delimiter::Semicolon);
        assert_eq!(Delimiter::guess("a\tb"), Delimiter::Tab);
        assert_eq!(Delimiter::guess("a,b"), Delimiter::Comma);
    }

    #[test]
    fn builds_inserts() {
        let cols = [col("id"), col("name"), col("note")];
        let rows = vec![
            vec!["1".to_string(), "it's".into()],
            vec!["2".into(), "".into()],
        ];
        let s = insert_statements(
            Driver::Postgres,
            "public.t",
            &cols,
            &[Some(0), Some(1), None],
            &rows,
            true,
        );
        assert_eq!(s, vec!["INSERT INTO public.t (\"id\", \"name\")\nVALUES ('1', 'it''s'),\n       ('2', NULL)".to_string()]);
        let s = insert_statements(
            Driver::Oracle,
            "T",
            &cols,
            &[Some(0), None, Some(5)],
            &rows,
            false,
        );
        assert_eq!(s.len(), 2);
        assert_eq!(s[1], "INSERT INTO T (\"id\", \"note\")\nVALUES ('2', NULL)");
        let many: Vec<Vec<String>> = (0..250).map(|i| vec![i.to_string()]).collect();
        assert_eq!(
            insert_statements(
                Driver::MsSql,
                "t",
                &cols,
                &[Some(0), None, None],
                &many,
                true
            )
            .len(),
            3
        );
        assert!(
            insert_statements(Driver::Sqlite, "t", &cols, &[None, None, None], &many, true)
                .is_empty()
        );
    }
}
