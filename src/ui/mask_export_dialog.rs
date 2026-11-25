//! "Export with Masking…": exports or copies a result with a masking rule per column (fake
//! names, emails and phones, hash, last 4…), the same deterministic rules as the table copy.

use crate::db::copy::Mask;
use crate::i18n::{tr, trf};
use crate::ui::results::ExportFormat;
use crate::ui::theme::{self, palette};
use egui::{Margin, RichText};

pub struct MaskExportDialog {
    /// Console whose last result is exported.
    pub console: u64,
    columns: Vec<String>,
    pub masks: Vec<Mask>,
    pub format: ExportFormat,
    /// Quoted table for "SQL Inserts" and the suggested file name.
    pub insert_target: String,
    pub base_name: String,
}

pub enum MaskExportAction {
    Save,
    Copy,
    Close,
}

impl MaskExportDialog {
    /// Opens with the masks suggested by the column names.
    pub fn new(
        console: u64,
        columns: Vec<String>,
        format: ExportFormat,
        insert_target: String,
        base_name: String,
    ) -> Self {
        let masks = columns.iter().map(|c| Mask::suggest(c)).collect();
        Self {
            console,
            columns,
            masks,
            format,
            insert_target,
            base_name,
        }
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Option<MaskExportAction> {
        let mut result = None;
        let p = palette(ctx.global_style().visuals.dark_mode);
        let frame = egui::Frame::popup(&ctx.global_style())
            .fill(p.popup_bg)
            .corner_radius(12)
            .inner_margin(Margin::same(20))
            .stroke(egui::Stroke::new(1.0, p.popup_border));
        egui::Modal::new(egui::Id::new("mask_export_dialog")).frame(frame).show(ctx, |ui| {
            ui.set_width(460.0_f32.min(ctx.content_rect().width() - 60.0).max(300.0));
            ui.label(RichText::new(tr("Export with Masking")).font(theme::bold(15.0)).color(p.text));
            ui.label(RichText::new(tr("The grid keeps the real values; only the exported text is masked.")).color(p.muted));
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.label(tr("Format:"));
                egui::ComboBox::from_id_salt("mask_export_format").selected_text(self.format.label()).width(140.0).show_ui(ui, |ui| {
                    for f in ExportFormat::ALL {
                        ui.selectable_value(&mut self.format, f, f.label());
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(tr("Suggest masking")).clicked() {
                        for (c, m) in self.columns.iter().zip(&mut self.masks) {
                            *m = Mask::suggest(c);
                        }
                    }
                });
            });
            ui.add_space(8.0);
            egui::ScrollArea::vertical().id_salt("mask_export_columns").max_height(280.0).auto_shrink([false, true]).show(ui, |ui| {
                egui::Grid::new("mask_export_grid").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
                    for (i, (c, mask)) in self.columns.iter().zip(self.masks.iter_mut()).enumerate() {
                        ui.allocate_ui_with_layout(egui::vec2(200.0, 20.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.set_min_width(200.0);
                            ui.add(egui::Label::new(RichText::new(c).color(p.text)).truncate());
                        });
                        egui::ComboBox::from_id_salt(("mask_export", i)).selected_text(mask.label()).width(150.0).show_ui(ui, |ui| {
                            for m in Mask::ALL {
                                ui.selectable_value(mask, m, m.label());
                            }
                        });
                        ui.end_row();
                    }
                });
            });
            let masked = self.masks.iter().filter(|m| **m != Mask::Keep).count();
            ui.add_space(6.0);
            ui.label(RichText::new(trf("Masked columns: {n}", &[("n", &masked)])).small().color(p.muted));
            ui.label(
                RichText::new(tr("Masking is deterministic: the same value always gives the same masked value, using a key kept in this computer's keychain."))
                    .small()
                    .color(p.muted),
            );
            ui.add_space(16.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                if theme::primary_button(ui, &trf("Export to {format} file…", &[("format", &self.format.label())])).clicked() {
                    result = Some(MaskExportAction::Save);
                }
                if theme::secondary_button(ui, tr("Copy to clipboard")).clicked() {
                    result = Some(MaskExportAction::Copy);
                }
                if theme::secondary_button(ui, tr("Cancel")).clicked() {
                    result = Some(MaskExportAction::Close);
                }
            });
        });
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            result = Some(MaskExportAction::Close);
        }
        result
    }
}
