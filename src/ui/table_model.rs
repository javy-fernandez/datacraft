//! Draggable catalog model and a standalone vector export of the same layout.

use crate::db::table_model::Model;
use crate::i18n::tr;
use crate::ui::theme::pal;
use egui::{FontId, Pos2, Rect, Sense, Stroke, Vec2, pos2, vec2};

pub struct ModelView {
    model: Model,
    positions: Vec<Pos2>,
    endpoints: Vec<Option<(usize, usize)>>,
}

const WIDTH: f32 = 280.0;
fn height(columns: usize) -> f32 {
    42.0 + columns as f32 * 21.0
}

impl ModelView {
    pub fn new(model: Model) -> Self {
        let mut positions = Vec::new();
        let mut y = 20.0;
        for row in model.tables.chunks(3) {
            for column in 0..row.len() {
                positions.push(pos2(20.0 + column as f32 * 350.0, y));
            }
            y += row.iter().map(|t| height(t.columns.len())).fold(0.0, f32::max) + 70.0;
        }
        let indices: std::collections::HashMap<_, _> = model.tables.iter().enumerate().map(|(i, t)| ((t.schema.as_str(), t.name.as_str()), i)).collect();
        let endpoints = model
            .relations
            .iter()
            .map(|r| Some((*indices.get(&(r.schema.as_str(), r.table.as_str()))?, *indices.get(&(r.ref_schema.as_str(), r.ref_table.as_str()))?)))
            .collect();
        Self { model, positions, endpoints }
    }

    pub fn model(&self) -> &Model {
        &self.model
    }

    fn size(&self) -> Vec2 {
        self.model
            .tables
            .iter()
            .zip(&self.positions)
            .fold(vec2(900.0, 420.0), |size, (t, p)| size.max(p.to_vec2() + vec2(WIDTH + 30.0, height(t.columns.len()) + 30.0)))
    }

    fn relationship_points(&self, index: usize) -> Option<Vec<Pos2>> {
        let (child, parent) = self.endpoints.get(index).copied().flatten()?;
        let start = self.positions.get(child)?;
        let end = self.positions.get(parent)?;
        let (a, b) = (*start + vec2(WIDTH, 21.0), *end + vec2(0.0, 21.0));
        if child == parent {
            let top = start.y;
            let y = (top - 14.0).max(2.0);
            let left = (b.x - 14.0).max(2.0);
            return Some(vec![a, pos2(a.x + 24.0, a.y), pos2(a.x + 24.0, y), pos2(left, y), pos2(left, b.y), b]);
        }
        let bend = (a.x + b.x) / 2.0;
        Some(vec![a, pos2(bend, a.y), pos2(bend, b.y), b])
    }

    pub fn show(&mut self, ui: &mut egui::Ui) {
        ui.label(tr("Drag tables to arrange the model. Lines represent declared foreign keys."));
        let p = pal(ui);
        egui::ScrollArea::both().id_salt("table_model_canvas").max_height(460.0).show(ui, |ui| {
            let (canvas, _) = ui.allocate_exact_size(self.size(), Sense::hover());
            let origin = canvas.min.to_vec2();
            for index in 0..self.model.relations.len() {
                if let Some(points) = self.relationship_points(index) {
                    let points: Vec<_> = points.into_iter().map(|p| p + origin).collect();
                    if let Some(&parent) = points.last() {
                        ui.painter().circle_filled(parent, 3.5, p.accent);
                    }
                    ui.painter().add(egui::Shape::line(points, Stroke::new(1.5, p.accent)));
                }
            }
            for (index, (table, position)) in self.model.tables.iter().zip(&mut self.positions).enumerate() {
                let rect = Rect::from_min_size(*position + origin, vec2(WIDTH, height(table.columns.len())));
                let response = ui.interact(rect, ui.id().with(("model_table", index)), Sense::drag());
                response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, ui.is_enabled(), format!("{}.{}", table.schema, table.name)));
                if response.dragged() {
                    *position = (*position + response.drag_delta()).max(pos2(10.0, 10.0));
                }
                ui.painter().rect_filled(rect, 6.0, p.popup_bg);
                ui.painter().rect_stroke(rect, 6.0, Stroke::new(1.0, p.control_border), egui::StrokeKind::Inside);
                let clipped = ui.painter().with_clip_rect(rect.shrink(4.0));
                clipped.text(
                    rect.min + vec2(10.0, 12.0),
                    egui::Align2::LEFT_TOP,
                    format!("{}.{}", table.schema, table.name),
                    FontId::proportional(13.0),
                    p.text,
                );
                ui.painter().hline(rect.x_range(), rect.top() + 35.0, Stroke::new(1.0, p.control_border));
                for (row, column) in table.columns.iter().enumerate() {
                    clipped.text(
                        rect.min + vec2(10.0, 42.0 + row as f32 * 21.0),
                        egui::Align2::LEFT_TOP,
                        format!(
                            "{}{}  {}{}",
                            if column.primary_key { "PK " } else { "" },
                            column.name,
                            column.data_type,
                            if column.nullable { "" } else { " · NOT NULL" }
                        ),
                        FontId::monospace(11.0),
                        p.text,
                    );
                }
            }
        });
        if self.model.relations.is_empty() {
            ui.label(tr("No declared foreign keys were found for these tables."));
        }
        for relation in &self.model.relations {
            ui.monospace(format!(
                "{}.{} ({}) → {}.{} ({})",
                relation.schema,
                relation.table,
                relation.columns.join(", "),
                relation.ref_schema,
                relation.ref_table,
                relation.ref_columns.join(", ")
            ));
        }
    }

    pub fn svg(&self) -> String {
        use std::fmt::Write;
        let size = self.size();
        let mut out = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\"><rect width=\"100%\" height=\"100%\" fill=\"#f6f8fa\"/>",
            size.x, size.y, size.x, size.y
        );
        for index in 0..self.model.relations.len() {
            if let Some(points) = self.relationship_points(index) {
                let mut path = String::new();
                for (i, point) in points.iter().enumerate() {
                    let _ = write!(path, "{}{} {} ", if i == 0 { "M" } else { "L" }, point.x, point.y);
                }
                let _ = write!(out, "<path d=\"{path}\" fill=\"none\" stroke=\"#1685ad\" stroke-width=\"2\"/>");
            }
        }
        for (table, position) in self.model.tables.iter().zip(&self.positions) {
            let _ = write!(
                out,
                "<g transform=\"translate({},{})\"><rect width=\"{WIDTH}\" height=\"{}\" rx=\"6\" fill=\"white\" stroke=\"#657080\"/><g fill=\"#17212b\" font-family=\"sans-serif\" font-size=\"12\"><text x=\"10\" y=\"23\" font-weight=\"bold\">{}.{}</text>",
                position.x,
                position.y,
                height(table.columns.len()),
                escape(&table.schema),
                escape(&table.name)
            );
            for (row, c) in table.columns.iter().enumerate() {
                let _ = write!(
                    out,
                    "<text x=\"10\" y=\"{}\">{}{}: {}{}</text>",
                    57 + row * 21,
                    if c.primary_key { "PK " } else { "" },
                    escape(&c.name),
                    escape(&c.data_type),
                    if c.nullable { "" } else { " NOT NULL" }
                );
            }
            out.push_str("</g></g>");
        }
        out.push_str("</svg>");
        out
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::table_model::{ModelColumn, ModelTable};

    #[test]
    fn cached_endpoints_distinguish_schemas_and_follow_dragged_positions() {
        let tables = ["one", "two"].into_iter().map(|schema| ModelTable { schema: schema.into(), name: "nodes".into(), columns: vec![] }).collect();
        let relation = crate::db::table_model::Relation {
            schema: "two".into(),
            table: "nodes".into(),
            ref_schema: "one".into(),
            ref_table: "nodes".into(),
            columns: vec!["parent".into()],
            ref_columns: vec!["id".into()],
        };
        let mut view = ModelView::new(Model { tables, relations: vec![relation] });
        let points = view.relationship_points(0).unwrap();
        assert_eq!(points.first(), Some(&pos2(650.0, 41.0)));
        assert_eq!(points.last(), Some(&pos2(20.0, 41.0)));
        view.positions[1] += vec2(30.0, 50.0);
        let moved = view.relationship_points(0).unwrap();
        assert_eq!(moved.first(), Some(&pos2(680.0, 91.0)));
        assert_eq!(moved.last(), Some(&pos2(20.0, 41.0)));
    }
    #[test]
    fn svg_keeps_self_referencing_relationships_visible_outside_the_table() {
        let relation = crate::db::table_model::Relation {
            schema: "s".into(),
            table: "nodes".into(),
            columns: vec!["parent".into()],
            ref_schema: "s".into(),
            ref_table: "nodes".into(),
            ref_columns: vec!["id".into()],
        };
        let view = ModelView::new(Model { tables: vec![ModelTable { schema: "s".into(), name: "nodes".into(), columns: vec![] }], relations: vec![relation] });
        let svg = view.svg();
        let path = svg.split("<path d=\"").nth(1).unwrap().split('"').next().unwrap().replace(['M', 'L'], " ");
        let values: Vec<f32> = path.split_whitespace().map(|v| v.parse().unwrap()).collect();
        assert!(values.iter().skip(1).step_by(2).any(|y| *y < view.positions[0].y), "self-reference is hidden behind its own table");
    }

    #[test]
    fn model_svg_is_standalone_and_escapes_catalog_text() {
        let view = ModelView::new(Model {
            tables: vec![ModelTable {
                schema: "a&b".into(),
                name: "<script>alert(1)</script>".into(),
                columns: vec![ModelColumn { name: "x<y".into(), data_type: "TEXT".into(), nullable: false, primary_key: true }],
            }],
            relations: vec![],
        });
        let svg = view.svg();
        assert!(svg.starts_with("<svg xmlns="));
        assert!(svg.ends_with("</svg>"));
        assert!(!svg.contains("<script>"));
        assert!(svg.contains("a&amp;b"));
        assert!(svg.contains("x&lt;y"));
        assert!(svg.contains("PK "));
    }
}
