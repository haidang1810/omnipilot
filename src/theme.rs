//! Look & feel: colours, fonts and small reusable widgets.

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Frame, Margin, RichText, Stroke, TextStyle, Ui, vec2};

pub const BG: Color32 = Color32::from_rgb(0x0d, 0x10, 0x16);
pub const PANEL: Color32 = Color32::from_rgb(0x12, 0x16, 0x1e);
pub const CARD: Color32 = Color32::from_rgb(0x18, 0x1d, 0x28);
pub const CARD_HI: Color32 = Color32::from_rgb(0x20, 0x26, 0x34);
pub const BORDER: Color32 = Color32::from_rgb(0x29, 0x30, 0x40);
pub const INPUT: Color32 = Color32::from_rgb(0x0f, 0x13, 0x1b);
pub const TEXT: Color32 = Color32::from_rgb(0xe7, 0xea, 0xf1);
pub const MUTED: Color32 = Color32::from_rgb(0x8a, 0x93, 0xa7);
pub const ACCENT: Color32 = Color32::from_rgb(0x72, 0x5e, 0xfa);
pub const GREEN: Color32 = Color32::from_rgb(0x34, 0xd3, 0x99);
pub const RED: Color32 = Color32::from_rgb(0xf8, 0x71, 0x71);
pub const YELLOW: Color32 = Color32::from_rgb(0xfb, 0xbf, 0x24);

/// Inner padding of every text box (egui's default is very tight).
pub const INPUT_MARGIN: Margin = Margin::symmetric(10, 8);

pub fn bold() -> FontFamily {
    FontFamily::Name("bold".into())
}

pub fn apply(ctx: &egui::Context) {
    install_fonts(ctx);

    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = PANEL;
    v.extreme_bg_color = INPUT;
    v.faint_bg_color = CARD;
    v.code_bg_color = INPUT;
    v.hyperlink_color = ACCENT;
    v.selection.bg_fill = Color32::from_rgb(0x4a, 0x3d, 0xb8);
    v.selection.stroke = Stroke::new(1.0_f32, TEXT);
    v.window_corner_radius = CornerRadius::same(12);
    v.menu_corner_radius = CornerRadius::same(10);
    v.window_stroke = Stroke::new(1.0_f32, BORDER);

    let w = &mut v.widgets;
    for wv in [&mut w.noninteractive, &mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
        wv.corner_radius = CornerRadius::same(8);
        wv.expansion = 0.0;
    }
    w.noninteractive.bg_fill = CARD;
    w.noninteractive.weak_bg_fill = CARD;
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    w.inactive.bg_fill = CARD_HI;
    w.inactive.weak_bg_fill = CARD_HI;
    w.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    w.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    w.hovered.bg_fill = Color32::from_rgb(0x2a, 0x31, 0x44);
    w.hovered.weak_bg_fill = Color32::from_rgb(0x2a, 0x31, 0x44);
    w.hovered.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    w.hovered.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
    w.active.bg_fill = ACCENT;
    w.active.weak_bg_fill = ACCENT;
    w.active.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    w.active.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
    w.open.bg_fill = CARD_HI;
    w.open.weak_bg_fill = CARD_HI;
    w.open.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    ctx.set_visuals(v);

    ctx.style_mut(|s| {
        s.spacing.item_spacing = vec2(8.0, 8.0);
        s.spacing.button_padding = vec2(12.0, 6.0);
        s.spacing.interact_size.y = 30.0;
        s.spacing.combo_width = 220.0;
        s.text_styles = [
            (TextStyle::Heading, FontId::new(22.0, bold())),
            (TextStyle::Body, FontId::proportional(15.0)),
            (TextStyle::Button, FontId::proportional(15.0)),
            (TextStyle::Small, FontId::proportional(12.5)),
            (TextStyle::Monospace, FontId::monospace(13.0)),
        ]
        .into();
    });
}

fn install_fonts(ctx: &egui::Context) {
    // egui's built-in font lacks many Vietnamese glyphs; use Segoe UI from Windows.
    let fonts_dir = PathBuf::from(std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into())).join("Fonts");
    let mut fonts = egui::FontDefinitions::default();
    let mut load = |name: &str, file: &str| -> bool {
        match std::fs::read(fonts_dir.join(file)) {
            Ok(bytes) => {
                fonts.font_data.insert(name.into(), Arc::new(egui::FontData::from_owned(bytes)));
                true
            }
            Err(_) => false,
        }
    };
    let regular = load("segoe", "segoeui.ttf");
    let semibold = load("segoe-sb", "seguisb.ttf");
    let mono = load("consolas", "consola.ttf");
    if regular {
        fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "segoe".into());
    }
    if mono {
        fonts.families.entry(FontFamily::Monospace).or_default().insert(0, "consolas".into());
    }
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    // Bold family: semibold first, then everything Proportional has (incl. icons).
    let mut bold_list = Vec::new();
    if semibold {
        bold_list.push("segoe-sb".to_string());
    }
    bold_list.extend(fonts.families[&FontFamily::Proportional].iter().cloned());
    fonts.families.insert(bold(), bold_list);
    ctx.set_fonts(fonts);
}

pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> egui::InnerResponse<R> {
    card_frame(BORDER).show(ui, add)
}

pub fn card_frame(border: Color32) -> Frame {
    Frame::new().fill(CARD).stroke(Stroke::new(1.0_f32, border)).corner_radius(12).inner_margin(Margin::same(16))
}

pub fn title(text: impl Into<String>) -> RichText {
    RichText::new(text).family(bold()).size(16.0).color(TEXT)
}

pub fn muted(text: impl Into<String>) -> RichText {
    RichText::new(text).color(MUTED)
}

/// Filled, coloured call-to-action button.
pub fn button(text: impl Into<String>, fill: Color32) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text).family(bold()).color(Color32::WHITE)).fill(fill).stroke(Stroke::NONE).min_size(vec2(0.0, 34.0))
}

/// Small rounded label.
pub fn chip(ui: &mut Ui, text: impl Into<String>, color: Color32) {
    Frame::new()
        .fill(color.gamma_multiply(0.14))
        .corner_radius(20)
        .inner_margin(Margin::symmetric(9, 3))
        .show(ui, |ui| ui.label(RichText::new(text).size(12.5).color(color)));
}

pub fn status_dot(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(10.0, 18.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.5, color);
}
