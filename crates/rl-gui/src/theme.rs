//! Colours and small widgets matching the Penpot mockups ("RemoteLinux UI" page).

use eframe::egui::{self, Color32, CornerRadius, Margin, Response, RichText, Stroke, Ui, Vec2};

pub const BG: Color32 = Color32::from_rgb(0x0F, 0x11, 0x15);
pub const SURFACE: Color32 = Color32::from_rgb(0x17, 0x1A, 0x21);
pub const SURFACE2: Color32 = Color32::from_rgb(0x1F, 0x23, 0x2C);
pub const BORDER: Color32 = Color32::from_rgb(0x2A, 0x2F, 0x3A);
pub const TEXT: Color32 = Color32::from_rgb(0xE6, 0xE8, 0xEE);
pub const MUTED: Color32 = Color32::from_rgb(0x8B, 0x93, 0xA5);
pub const ACCENT: Color32 = Color32::from_rgb(0x4F, 0x8C, 0xFF);
pub const SUCCESS: Color32 = Color32::from_rgb(0x3D, 0xDC, 0x97);
pub const SUCCESS_SOFT: Color32 = Color32::from_rgb(0x15, 0x33, 0x2A);
pub const WARN: Color32 = Color32::from_rgb(0xF5, 0xB9, 0x4A);
pub const DANGER: Color32 = Color32::from_rgb(0xFF, 0x5C, 0x6C);
pub const DANGER_SOFT: Color32 = Color32::from_rgb(0x3A, 0x1E, 0x24);

pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.style_mut_of(egui::Theme::Dark, |s| {
        let v = &mut s.visuals;
        v.panel_fill = BG;
        v.window_fill = SURFACE;
        v.extreme_bg_color = BG;
        v.faint_bg_color = SURFACE2;
        v.hyperlink_color = ACCENT;
        v.selection.bg_fill = ACCENT;
        v.selection.stroke = Stroke::new(1.0, Color32::WHITE);
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        for (w, fill) in [
            (&mut v.widgets.inactive, SURFACE2),
            (&mut v.widgets.hovered, Color32::from_rgb(0x28, 0x2E, 0x3A)),
            (&mut v.widgets.active, Color32::from_rgb(0x30, 0x37, 0x45)),
            (&mut v.widgets.open, SURFACE2),
        ] {
            w.bg_fill = fill;
            w.weak_bg_fill = fill;
            w.bg_stroke = Stroke::new(1.0, BORDER);
            w.fg_stroke = Stroke::new(1.0, TEXT);
            w.corner_radius = CornerRadius::same(8);
        }
        s.spacing.item_spacing = Vec2::new(8.0, 8.0);
        s.spacing.button_padding = Vec2::new(14.0, 8.0);
        s.spacing.interact_size.y = 32.0;
        use egui::{FontFamily::*, FontId, TextStyle};
        s.text_styles = [
            (TextStyle::Heading, FontId::new(26.0, Proportional)),
            (TextStyle::Body, FontId::new(15.0, Proportional)),
            (TextStyle::Button, FontId::new(15.0, Proportional)),
            (TextStyle::Small, FontId::new(12.0, Proportional)),
            (TextStyle::Monospace, FontId::new(14.0, Monospace)),
        ]
        .into();
    });
}

/// Rounded panel with a border, like the host cards in the mockups.
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(16)
        .inner_margin(Margin::same(24))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// Lays out `add` in a centred column at most `max` wide.
pub fn column(ui: &mut Ui, max: f32, add: impl FnOnce(&mut Ui)) {
    let w = ui.available_width().min(max);
    let pad = ((ui.available_width() - w) / 2.0).max(0.0);
    ui.horizontal_top(|ui| {
        ui.add_space(pad);
        ui.vertical(|ui| {
            ui.set_width(w);
            add(ui)
        });
    });
}

pub fn primary(ui: &mut Ui, text: &str, enabled: bool) -> Response {
    let label = RichText::new(text).color(Color32::WHITE).strong().size(16.0);
    ui.add_enabled(enabled, egui::Button::new(label).fill(ACCENT).corner_radius(10).min_size(Vec2::new(150.0, 46.0)))
}

pub fn secondary(ui: &mut Ui, text: &str) -> Response {
    ui.add(egui::Button::new(RichText::new(text).strong()).corner_radius(10).min_size(Vec2::new(0.0, 36.0)))
}

pub fn danger(ui: &mut Ui, text: &str) -> Response {
    let label = RichText::new(text).color(DANGER).strong();
    ui.add(egui::Button::new(label).fill(DANGER_SOFT).corner_radius(10).min_size(Vec2::new(0.0, 36.0)))
}

/// Small coloured status badge with a dot ("• Online").
pub fn pill(ui: &mut Ui, text: &str, fg: Color32, bg: Color32) {
    egui::Frame::new().fill(bg).corner_radius(12).inner_margin(Margin::symmetric(10, 3)).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            dot(ui, fg);
            ui.label(RichText::new(text).color(fg).size(12.0).strong());
        });
    });
}

/// Filled status dot; drawn rather than typed because the default fonts lack "●".
pub fn dot(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

/// Single-choice row of buttons. Returns true when the value changed.
pub fn segmented<T: PartialEq + Clone>(ui: &mut Ui, value: &mut T, options: &[(T, &str)]) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (opt, label) in options {
            let on = *value == *opt;
            let text = RichText::new(*label).strong().color(if on { Color32::WHITE } else { MUTED });
            let btn = egui::Button::new(text).fill(if on { ACCENT } else { BG }).corner_radius(8).min_size(Vec2::new(64.0, 32.0));
            if ui.add(btn).clicked() && !on {
                *value = opt.clone();
                changed = true;
            }
        }
    });
    changed
}

pub fn heading(ui: &mut Ui, title: &str, sub: &str) {
    ui.add_space(24.0);
    ui.label(RichText::new(title).size(28.0).strong().color(TEXT));
    ui.label(RichText::new(sub).color(MUTED));
    ui.add_space(16.0);
}

pub fn muted(text: impl Into<String>) -> RichText {
    RichText::new(text).color(MUTED)
}
