//! "Run SQL Script…": progress of the files being run, the failing statement's question (Retry,
//! Skip, Skip All, Stop) and, at the end, how many failed and where.

use crate::db::script::{Decision, Progress, ScriptRun};
use crate::i18n::{tr, trf};
use crate::ui::object_browser::format_bytes;
use crate::ui::theme::{self, palette};
use crate::worker::ConnId;
use egui::{Align, Layout, RichText, Stroke};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

pub struct ScriptDialog {
    pub conn: ConnId,
    /// Connection, and database when run from one.
    pub target: String,
    pub files: Vec<PathBuf>,
    pub run: Arc<ScriptRun>,
}

impl ScriptDialog {
    /// Some(true) when closed after statements ran (the explorer is refreshed).
    pub fn show(&mut self, ctx: &egui::Context) -> Option<bool> {
        let p = palette(ctx.global_style().visuals.dark_mode);
        let progress = self.run.snapshot();
        let mut close = false;
        let frame = egui::Frame::popup(&ctx.global_style())
            .fill(p.popup_bg)
            .corner_radius(12)
            .inner_margin(egui::Margin::same(20))
            .stroke(Stroke::new(1.0, p.popup_border));
        egui::Modal::new(egui::Id::new("run_script"))
            .frame(frame)
            .show(ctx, |ui| {
                ui.set_width(560.0);
                ui.label(RichText::new(tr("Run SQL Script")).font(theme::bold(15.0)));
                ui.label(RichText::new(&self.target).color(p.muted));
                ui.add_space(12.0);
                let name = self
                    .files
                    .get(progress.file)
                    .and_then(|f| f.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if self.files.len() > 1 {
                    ui.label(trf(
                        "File {n} of {total}: {name}",
                        &[
                            ("n", &(progress.file + 1)),
                            ("total", &self.files.len()),
                            ("name", &name),
                        ],
                    ));
                } else {
                    ui.label(&name);
                }
                ui.add_space(6.0);
                let fraction = if progress.bytes_total == 0 {
                    1.0
                } else {
                    progress.bytes_done as f32 / progress.bytes_total as f32
                };
                let bytes = trf(
                    "{done} of {total}",
                    &[
                        ("done", &format_bytes(progress.bytes_done)),
                        ("total", &format_bytes(progress.bytes_total)),
                    ],
                );
                ui.add(
                    egui::ProgressBar::new(fraction)
                        .fill(p.selection)
                        .text(RichText::new(bytes).color(p.text))
                        .animate(!progress.finished && progress.waiting.is_none()),
                );
                ui.add_space(6.0);
                let mut counts = trf("{count} statements run", &[("count", &progress.statements)]);
                if !progress.failed.is_empty() {
                    counts += &format!(
                        " · {}",
                        trf("{count} failed", &[("count", &progress.failed.len())])
                    );
                }
                ui.label(RichText::new(counts).color(p.muted));

                if let Some(f) = &progress.waiting {
                    ui.add_space(12.0);
                    ui.label(
                        RichText::new(trf(
                            "Line {line} of {file} failed:",
                            &[("line", &f.line), ("file", &f.file)],
                        ))
                        .strong(),
                    );
                    ui.add(egui::Label::new(RichText::new(&f.error).color(p.error)).wrap());
                    ui.add_space(4.0);
                    egui::Frame::NONE
                        .fill(p.editor_bg)
                        .stroke(Stroke::new(1.0, p.control_border))
                        .corner_radius(6)
                        .inner_margin(8)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            egui::ScrollArea::vertical()
                                .max_height(120.0)
                                .show(ui, |ui| {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(preview(&f.statement))
                                                .monospace()
                                                .size(12.0),
                                        )
                                        .wrap(),
                                    );
                                });
                        });
                    ui.add_space(12.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        for (label, d) in [
                            (tr("Stop"), Decision::Stop),
                            (tr("Skip All"), Decision::SkipAll),
                            (tr("Skip"), Decision::Skip),
                        ] {
                            if theme::secondary_button(ui, label).clicked() {
                                self.run.decide(d);
                            }
                        }
                        if theme::primary_button(ui, tr("Retry")).clicked() {
                            self.run.decide(Decision::Retry);
                        }
                    });
                    return;
                }

                if progress.finished {
                    ui.add_space(12.0);
                    summary(ui, &progress, &p);
                }
                ui.add_space(16.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if progress.finished {
                        close = theme::primary_button(ui, tr("Close")).clicked();
                    } else {
                        let cancelling = self.run.cancel.load(Ordering::SeqCst);
                        if ui
                            .add_enabled_ui(!cancelling, |ui| {
                                theme::secondary_button(ui, tr("Cancel"))
                            })
                            .inner
                            .clicked()
                        {
                            self.run.cancel.store(true, Ordering::SeqCst);
                        }
                        if cancelling {
                            ui.label(
                                RichText::new(tr("Cancelling after the current statement…"))
                                    .color(p.muted),
                            );
                        }
                    }
                });
            });
        if !progress.finished {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        close.then_some(progress.statements > 0)
    }
}

fn summary(ui: &mut egui::Ui, progress: &Progress, p: &theme::Palette) {
    match &progress.stopped {
        Some(reason) => {
            ui.label(RichText::new(trf("Stopped: {reason}", &[("reason", reason)])).color(p.error))
        }
        None if progress.failed.is_empty() => {
            ui.label(RichText::new(tr("Finished without errors")).color(p.ok))
        }
        None => ui.label(RichText::new(tr("Finished with errors")).strong()),
    };
    if progress.failed.is_empty() {
        return;
    }
    ui.add_space(6.0);
    egui::ScrollArea::vertical()
        .max_height(180.0)
        .show(ui, |ui| {
            for f in &progress.failed {
                let first = f.error.lines().next().unwrap_or_default();
                ui.add(
                    egui::Label::new(
                        RichText::new(format!("{}:{} — {first}", f.file, f.line)).size(12.0),
                    )
                    .truncate(),
                )
                .on_hover_text(&f.error);
            }
        });
}

/// The start of a statement that can be very long (an `INSERT` of thousands of rows).
fn preview(statement: &str) -> String {
    const MAX: usize = 600;
    match statement.char_indices().nth(MAX) {
        Some((i, _)) => format!("{}…", &statement[..i]),
        None => statement.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::script::Failure;
    use egui::vec2;

    #[test]
    #[ignore = "Requires a GPU; renders the question about a failing statement and the summary"]
    fn render_script_dialog() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/dialog-snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        let failure = |line: usize, error: &str| Failure {
            file: "shop.sql".into(),
            line,
            statement: "INSERT INTO orders (id, customer, total) VALUES (1042, 'ACME', 99.5);"
                .into(),
            error: error.into(),
        };
        let waiting = Progress {
            bytes_done: 40 << 20,
            bytes_total: 96 << 20,
            statements: 18_250,
            failed: vec![failure(212, "relation \"orders\" does not exist")],
            waiting: Some(failure(
                18_406,
                "duplicate key value violates unique constraint \"orders_pkey\"",
            )),
            ..Default::default()
        };
        let done = Progress {
            bytes_done: 96 << 20,
            bytes_total: 96 << 20,
            statements: 41_877,
            failed: vec![
                failure(212, "relation \"orders\" does not exist"),
                failure(
                    18_406,
                    "duplicate key value violates unique constraint \"orders_pkey\"",
                ),
            ],
            finished: true,
            ..Default::default()
        };
        for (name, progress) in [("script_waiting", waiting), ("script_done", done)] {
            let run = Arc::new(ScriptRun::default());
            *run.progress.lock().unwrap() = progress;
            let dialog = ScriptDialog {
                conn: 1,
                target: "orders@localhost / shop".into(),
                files: vec![PathBuf::from("/tmp/shop.sql")],
                run,
            };
            let mut first = true;
            let mut h = egui_kittest::Harness::builder()
                .with_size(vec2(760.0, 560.0))
                .build_ui_state(
                    move |ui, d: &mut ScriptDialog| {
                        if std::mem::take(&mut first) {
                            theme::install_fonts(ui.ctx());
                            theme::apply(ui.ctx(), true);
                        } else {
                            d.show(ui.ctx());
                        }
                    },
                    dialog,
                );
            h.run_steps(4);
            h.render()
                .unwrap()
                .save(dir.join(format!("{name}.png")))
                .unwrap();
        }
    }
}
