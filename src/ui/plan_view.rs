//! Explain plan as a tree: one row per node with its object, rows, cost and time, and a bar for
//! the share of the plan the node itself takes (time when analyzed, else cost). The heaviest
//! nodes are highlighted.

use crate::db::plan::PlanNode;
use crate::i18n::{tr, trf};
use crate::ui::theme::pal;
use egui::{Align, Layout, RichText, Sense, Ui, vec2};

/// Share of the plan from which a node is highlighted.
const HEAVY: f64 = 0.3;
const NOTABLE: f64 = 0.1;

/// The plan; true when "Show as Text" is clicked.
pub fn show(ui: &mut Ui, plan: &PlanNode, salt: u64) -> bool {
    let p = pal(ui);
    let analyzed = plan.analyzed();
    let total = plan.total_weight(analyzed);
    let mut rows = Vec::new();
    flatten(plan, 0, &mut rows);
    let mut as_text = false;
    ui.horizontal(|ui| {
        let summary = match (analyzed, plan.time_ms, plan.cost) {
            (true, Some(t), _) => trf("Explain Analyze · {time} total", &[("time", &ms(t))]),
            (_, _, Some(c)) => trf("Estimated plan · cost {cost}", &[("cost", &number(c))]),
            _ => tr("Estimated plan").to_string(),
        };
        ui.label(RichText::new(summary).color(p.muted));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            as_text = ui.small_button(tr("Show as Text")).clicked();
        });
    });
    ui.add_space(4.0);
    egui::ScrollArea::both()
        .id_salt(("plan", salt))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Grid::new(("plan_grid", salt))
                .striped(true)
                .spacing([18.0, 6.0])
                .show(ui, |ui| {
                    let header = |ui: &mut Ui, text: &str| ui.label(RichText::new(text).strong());
                    header(ui, tr("Operation"));
                    header(ui, tr("Object"));
                    header(ui, tr("Est. rows"));
                    if analyzed {
                        header(ui, tr("Actual rows"));
                        header(ui, tr("Time"));
                    }
                    header(ui, tr("Cost"));
                    header(
                        ui,
                        if analyzed {
                            tr("Share of time")
                        } else {
                            tr("Share of cost")
                        },
                    );
                    header(ui, tr("Details"));
                    ui.end_row();
                    for (depth, node) in rows {
                        let share = if total > 0.0 {
                            node.own_weight(analyzed) / total
                        } else {
                            0.0
                        };
                        let color = if share >= HEAVY {
                            p.error
                        } else if share >= NOTABLE {
                            p.warn
                        } else {
                            p.text
                        };
                        ui.horizontal(|ui| {
                            ui.add_space(depth as f32 * 16.0);
                            if depth > 0 {
                                ui.label(RichText::new("└").color(p.muted));
                            }
                            let op = RichText::new(&node.op).color(color);
                            ui.label(if share >= NOTABLE { op.strong() } else { op });
                        });
                        ui.label(&node.object);
                        ui.label(opt(node.est_rows, number));
                        if analyzed {
                            let actual = ui.label(opt(node.actual_rows, number));
                            if let Some(loops) = node.loops.filter(|l| *l > 1.0) {
                                actual.on_hover_text(trf(
                                    "{loops} loops",
                                    &[("loops", &number(loops))],
                                ));
                            }
                            ui.label(opt(node.time_ms, ms));
                        }
                        ui.label(opt(node.cost, number));
                        ui.horizontal(|ui| {
                            let (rect, _) =
                                ui.allocate_exact_size(vec2(80.0, 10.0), Sense::hover());
                            ui.painter().rect_filled(rect, 3.0, p.grid);
                            let mut fill = rect;
                            fill.set_width(rect.width() * share.clamp(0.0, 1.0) as f32);
                            ui.painter().rect_filled(
                                fill,
                                3.0,
                                if share >= HEAVY {
                                    p.error
                                } else if share >= NOTABLE {
                                    p.warn
                                } else {
                                    p.accent
                                },
                            );
                            ui.label(
                                RichText::new(format!("{:.0}%", share * 100.0))
                                    .size(12.0)
                                    .color(p.muted),
                            );
                        });
                        if node.detail.is_empty() {
                            ui.label("");
                        } else {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&node.detail).size(12.0).color(p.muted),
                                )
                                .extend(),
                            );
                        }
                        ui.end_row();
                    }
                });
        });
    as_text
}

fn flatten<'a>(node: &'a PlanNode, depth: usize, out: &mut Vec<(usize, &'a PlanNode)>) {
    out.push((depth, node));
    for child in &node.children {
        flatten(child, depth + 1, out);
    }
}

fn opt(v: Option<f64>, f: fn(f64) -> String) -> String {
    v.map(f).unwrap_or_default()
}

/// `1,234` for whole numbers, `12.5` otherwise.
fn number(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        let digits = (v.abs() as u64).to_string();
        let mut out = String::new();
        for (i, c) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i).is_multiple_of(3) {
                out.push(',');
            }
            out.push(c);
        }
        if v < 0.0 { format!("-{out}") } else { out }
    } else {
        format!("{v:.2}")
    }
}

fn ms(v: f64) -> String {
    if v >= 1000.0 {
        format!("{:.2} s", v / 1000.0)
    } else {
        format!("{v:.2} ms")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_numbers() {
        assert_eq!(number(1234567.0), "1,234,567");
        assert_eq!(number(40.0), "40");
        assert_eq!(number(12.5), "12.50");
        assert_eq!(ms(3.456), "3.46 ms");
        assert_eq!(ms(2500.0), "2.50 s");
    }

    #[test]
    #[ignore = "Requires a GPU; renders an analyzed PostgreSQL plan"]
    fn render_plan() {
        let json = r#"[{"Plan": {"Node Type": "Sort", "Total Cost": 240.5, "Plan Rows": 40, "Actual Rows": 38, "Actual Total Time": 14.2, "Actual Loops": 1, "Sort Key": ["c.name"],
            "Plans": [{"Node Type": "Hash Join", "Join Type": "Inner", "Total Cost": 220.5, "Plan Rows": 40, "Actual Rows": 38, "Actual Total Time": 12.9, "Actual Loops": 1, "Hash Cond": "(o.customer_id = c.id)",
            "Plans": [
              {"Node Type": "Seq Scan", "Relation Name": "orders", "Alias": "o", "Total Cost": 180.0, "Plan Rows": 9000, "Actual Rows": 9120, "Actual Total Time": 10.1, "Actual Loops": 1, "Filter": "(total > '10'::numeric)"},
              {"Node Type": "Hash", "Total Cost": 30.0, "Plan Rows": 10, "Actual Rows": 10, "Actual Total Time": 0.6, "Actual Loops": 1,
               "Plans": [{"Node Type": "Index Scan", "Relation Name": "customers", "Index Name": "customers_pkey", "Alias": "c", "Total Cost": 28.0, "Plan Rows": 10, "Actual Rows": 5, "Actual Total Time": 0.25, "Actual Loops": 2, "Index Cond": "(id < 100)"}]}
            ]}]}, "Execution Time": 14.6}]"#;
        let rs = crate::db::ResultSet {
            columns: vec!["QUERY PLAN".into()],
            rows: vec![vec![Some(json.into())]],
            ..Default::default()
        };
        let plan = crate::db::plan::detect(&rs).unwrap();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/dialog-snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        for dark in [true, false] {
            let mut first = true;
            let plan = plan.clone();
            let mut h = egui_kittest::Harness::builder()
                .with_size(vec2(1100.0, 300.0))
                .build_ui(move |ui| {
                    if std::mem::take(&mut first) {
                        crate::ui::theme::install_fonts(ui.ctx());
                        crate::ui::theme::apply(ui.ctx(), dark);
                        return;
                    }
                    egui::CentralPanel::default().show(ui, |ui| {
                        show(ui, &plan, 1);
                    });
                });
            h.run_steps(4);
            h.render()
                .unwrap()
                .save(dir.join(if dark {
                    "plan_tree_dark.png"
                } else {
                    "plan_tree_light.png"
                }))
                .unwrap();
        }
    }
}
