//! Shared bounded frame for workflow forms; only the body scrolls.

use crate::ui::theme::{self, palette};
use egui::{Align, Layout, Margin, Rect, RichText, Sense, Stroke, Ui, UiBuilder, Vec2, pos2, vec2};

pub(crate) struct Regions {
    body: Rect,
    footer: Rect,
}

fn region(root: &mut Ui, rect: Rect, id: &'static str) -> Ui {
    let mut ui = root.new_child(UiBuilder::new().id_salt(id).max_rect(rect).layout(Layout::top_down(Align::Min)));
    ui.set_clip_rect(rect.intersect(root.clip_rect()));
    ui
}

impl Regions {
    pub(crate) fn body_ui(&self, root: &mut Ui) -> Ui {
        region(root, self.body, "workflow_body")
    }

    pub(crate) fn footer_ui(&self, root: &mut Ui) -> Ui {
        region(root, self.footer, "workflow_footer")
    }
}

pub(crate) fn show<R>(
    ctx: &egui::Context,
    id: egui::Id,
    title: &str,
    subtitle: &str,
    preferred_size: Vec2,
    add_contents: impl FnOnce(&mut Ui, Regions) -> R,
) -> egui::ModalResponse<R> {
    let p = palette(ctx.global_style().visuals.dark_mode);
    let size = preferred_size.min((ctx.content_rect().size() - vec2(32.0, 32.0)).max(Vec2::ZERO));
    let frame = egui::Frame::popup(&ctx.global_style()).fill(p.popup_bg).corner_radius(12).inner_margin(Margin::ZERO).stroke(Stroke::new(1.0, p.popup_border));
    egui::Modal::new(id).frame(frame).show(ctx, |root| {
        let (outer, _) = root.allocate_exact_size(size, Sense::hover());
        let header_bottom = (outer.top() + 62.0).min(outer.bottom());
        let footer_top = (outer.bottom() - 82.0).max(header_bottom);
        let header = Rect::from_min_max(outer.min, pos2(outer.right(), header_bottom));
        let footer = Rect::from_min_max(pos2(outer.left(), footer_top), outer.max);
        let body = Rect::from_min_max(pos2(outer.left(), header_bottom), pos2(outer.right(), footer_top));
        for y in [header_bottom, footer_top] {
            root.painter().hline(outer.x_range(), y, Stroke::new(1.0, p.popup_border));
        }
        let mut heading = region(root, header.shrink2(vec2(20.0, 9.0)), "workflow_heading");
        heading.add(egui::Label::new(RichText::new(title).font(theme::bold(19.0))).truncate()).on_hover_text(title);
        heading.add(egui::Label::new(RichText::new(subtitle).small().color(p.muted)).truncate()).on_hover_text(subtitle);
        add_contents(root, Regions { body: body.shrink2(vec2(20.0, 12.0)), footer: footer.shrink2(vec2(20.0, 9.0)) })
    })
}
