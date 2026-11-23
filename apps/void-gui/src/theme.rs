//! The look: the "void" palette, fonts and icons, and small drawing helpers shared by panels.

use std::sync::Arc;

use egui::{
    Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Response,
    RichText, Sense, Stroke, TextStyle, Ui, Vec2, epaint::Shadow,
};
use void_core::config::Theme;

/// Phosphor icon codepoints (regular weight; font vendored in assets/fonts, MIT).
pub mod icon {
    pub const ARROW_CLOCKWISE: &str = "\u{E036}";
    pub const ARROW_SQUARE_OUT: &str = "\u{E5DE}";
    pub const ARROWS_IN_LINE_VERTICAL: &str = "\u{E532}";
    pub const BRACKETS_CURLY: &str = "\u{E860}";
    pub const CARET_DOWN: &str = "\u{E136}";
    pub const CARET_RIGHT: &str = "\u{E13A}";
    pub const CROSSHAIR: &str = "\u{E1D6}";
    pub const FILE: &str = "\u{E230}";
    pub const FILE_C: &str = "\u{EB32}";
    pub const FILE_CPP: &str = "\u{EB2E}";
    pub const FILE_CSS: &str = "\u{EB34}";
    pub const FILE_HTML: &str = "\u{EB38}";
    pub const FILE_IMAGE: &str = "\u{EA24}";
    pub const FILE_INI: &str = "\u{EB33}";
    pub const FILE_JS: &str = "\u{EB24}";
    pub const FILE_JSX: &str = "\u{EB3A}";
    pub const FILE_LOCK: &str = "\u{E95C}";
    pub const FILE_MD: &str = "\u{ED50}";
    pub const FILE_PDF: &str = "\u{E702}";
    pub const FILE_PLUS: &str = "\u{E236}";
    pub const FILE_PY: &str = "\u{EB2C}";
    pub const FILE_RS: &str = "\u{EB28}";
    pub const FILE_SVG: &str = "\u{ED08}";
    pub const FILE_TS: &str = "\u{EB26}";
    pub const FILE_TSX: &str = "\u{EB3C}";
    pub const FILE_TXT: &str = "\u{EB35}";
    pub const FILE_ZIP: &str = "\u{E958}";
    pub const FOLDER_PLUS: &str = "\u{E258}";
    pub const BRAIN: &str = "\u{E74E}";
    pub const CHAT: &str = "\u{E16C}";
    pub const CHATS: &str = "\u{E17E}";
    pub const CHECK: &str = "\u{E182}";
    pub const COPY: &str = "\u{E1CA}";
    pub const CHECK_CIRCLE: &str = "\u{E184}";
    pub const CIRCLE: &str = "\u{E18A}";
    pub const FILE_CODE: &str = "\u{E914}";
    pub const FILE_TEXT: &str = "\u{E23A}";
    pub const GIT_DIFF: &str = "\u{E27C}";
    pub const HAND: &str = "\u{E298}";
    pub const PROHIBIT: &str = "\u{E3DE}";
    pub const ROBOT: &str = "\u{E762}";
    pub const SHIELD_CHECK: &str = "\u{E40C}";
    pub const SHIELD_WARNING: &str = "\u{E412}";
    pub const TERMINAL: &str = "\u{EAE8}";
    pub const X_CIRCLE: &str = "\u{E4F8}";
    pub const CPU: &str = "\u{E610}";
    pub const FLOPPY: &str = "\u{E248}";
    pub const FOLDER: &str = "\u{E24A}";
    pub const FOLDER_OPEN: &str = "\u{E256}";
    pub const GAUGE: &str = "\u{E628}";
    pub const GEAR: &str = "\u{E272}";
    pub const LAYOUT: &str = "\u{E6D6}";
    pub const LIGHTNING: &str = "\u{E2DE}";
    pub const MAGNIFIER: &str = "\u{E30C}";
    pub const PENCIL: &str = "\u{E3B4}";
    pub const PLANET: &str = "\u{E652}";
    pub const PLUGS: &str = "\u{EB5A}";
    pub const PLUS: &str = "\u{E3D4}";
    pub const SEND: &str = "\u{E396}";
    pub const SIGN_OUT: &str = "\u{E42A}";
    pub const SPARKLE: &str = "\u{E6A2}";
    pub const STAR: &str = "\u{E6A4}";
    pub const STOP: &str = "\u{E46C}";
    pub const TRASH: &str = "\u{E4A6}";
    pub const WARNING: &str = "\u{E4E0}";
}

/// The colours every panel draws with.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub bg_deep: Color32,
    pub bg_panel: Color32,
    pub bg_raised: Color32,
    pub bg_hover: Color32,
    pub border: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    pub text_faint: Color32,
    /// Violet: the main accent.
    pub accent: Color32,
    /// Cyan: the second accent (gradients, reasoning).
    pub accent2: Color32,
    /// Pink: a third accent (Anthropic badges).
    pub accent3: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub err: Color32,
    pub dark: bool,
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

pub fn palette(theme: Theme) -> Palette {
    match theme {
        Theme::Dark => Palette {
            bg_deep: hex(0x09090F),
            bg_panel: hex(0x10101A),
            bg_raised: hex(0x181826),
            bg_hover: hex(0x222236),
            border: hex(0x2A2A40),
            text: hex(0xE8E6F3),
            text_dim: hex(0xA09DB8),
            text_faint: hex(0x6C6985),
            accent: hex(0x8B5CF6),
            accent2: hex(0x22D3EE),
            accent3: hex(0xF472B6),
            ok: hex(0x34D399),
            warn: hex(0xFBBF24),
            err: hex(0xF87171),
            dark: true,
        },
        Theme::Light => Palette {
            bg_deep: hex(0xE9E8F2),
            bg_panel: hex(0xF7F6FC),
            bg_raised: hex(0xFFFFFF),
            bg_hover: hex(0xEDEBFA),
            border: hex(0xD6D3E6),
            text: hex(0x1A1830),
            text_dim: hex(0x55527A),
            text_faint: hex(0x8C89A8),
            accent: hex(0x7C3AED),
            accent2: hex(0x0891B2),
            accent3: hex(0xDB2777),
            ok: hex(0x059669),
            warn: hex(0xD97706),
            err: hex(0xDC2626),
            dark: false,
        },
    }
}

/// Linear blend of two colours (`t` in 0..=1).
pub fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(
        mix(a.r(), b.r()),
        mix(a.g(), b.g()),
        mix(a.b(), b.b()),
        mix(a.a(), b.a()),
    )
}

pub fn with_alpha(c: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha)
}

/// WCAG contrast ratio between two opaque colours.
#[cfg(test)]
pub fn contrast(a: Color32, b: Color32) -> f32 {
    fn lum(c: Color32) -> f32 {
        let ch = |v: u8| {
            let v = v as f32 / 255.0;
            if v <= 0.039_28 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * ch(c.r()) + 0.7152 * ch(c.g()) + 0.0722 * ch(c.b())
    }
    let (x, y) = (lum(a), lum(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

pub fn context_fraction(used: u32, limit: u32) -> f32 {
    if limit == 0 {
        0.0
    } else {
        (used as f32 / limit as f32).clamp(0.0, 1.0)
    }
}

/// Green while there's room, amber from 70%, red from 90%.
pub fn meter_color(p: &Palette, fraction: f32) -> Color32 {
    if fraction >= 0.9 {
        p.err
    } else if fraction >= 0.7 {
        p.warn
    } else {
        p.ok
    }
}

/// `fc-match -f '%{family}|%{file}'` output → the file, if fontconfig found the family asked for
/// (it silently substitutes another family otherwise).
pub fn parse_fc_match(out: &str, family: &str) -> Option<String> {
    let (fam, file) = out.trim().split_once('|')?;
    let wanted = family.to_lowercase();
    fam.split(',')
        .any(|f| f.trim().to_lowercase() == wanted)
        .then(|| file.to_string())
        .filter(|f| !f.is_empty())
}

fn system_font(pattern: &str, family: &str) -> Option<Vec<u8>> {
    let out = std::process::Command::new("fc-match")
        .args(["-f", "%{family}|%{file}", pattern])
        .output()
        .ok()?;
    let file = parse_fc_match(&String::from_utf8_lossy(&out.stdout), family)?;
    std::fs::read(file).ok()
}

pub const BOLD: &str = "void-bold";
/// Icons get a family of their own: Phosphor's private-use codepoints clash with glyphs in
/// other fonts (egui's icon font, Adwaita Sans, Nerd Fonts) that would otherwise win.
pub const ICONS: &str = "void-icons";

static BODY_SIZE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0x4170_0000); // 15.0

pub fn body_size() -> f32 {
    f32::from_bits(BODY_SIZE.load(std::sync::atomic::Ordering::Relaxed))
}

pub fn small_size() -> f32 {
    body_size() - 3.0
}

/// An icon on its own, in the icon family (size and colour as for any RichText).
pub fn ico(icon: &str) -> RichText {
    RichText::new(icon).family(FontFamily::Name(ICONS.into()))
}

/// Icon + text at `size` in `color` (`Color32::PLACEHOLDER` takes the widget's colour).
pub fn itext_styled(icon: &str, text: &str, size: f32, color: Color32) -> egui::WidgetText {
    use egui::text::{LayoutJob, TextFormat};
    let mut job = LayoutJob::default();
    job.append(
        icon,
        0.0,
        TextFormat {
            font_id: FontId::new(size, FontFamily::Name(ICONS.into())),
            color,
            valign: egui::Align::Center,
            ..Default::default()
        },
    );
    job.append(
        text,
        size * 0.45,
        TextFormat {
            font_id: FontId::proportional(size),
            color,
            valign: egui::Align::Center,
            ..Default::default()
        },
    );
    job.into()
}

/// Icon + text at body size, in the widget's colour.
pub fn itext(icon: &str, text: &str) -> egui::WidgetText {
    itext_styled(icon, text, body_size(), Color32::PLACEHOLDER)
}

/// Icon + text at small size, in the widget's colour.
pub fn itext_small(icon: &str, text: &str) -> egui::WidgetText {
    itext_styled(icon, text, small_size(), Color32::PLACEHOLDER)
}

/// Fonts: Adwaita Sans (an Inter derivative) and JetBrains Mono when installed, egui's own
/// fonts otherwise, and the bundled Phosphor icons as a fallback in every family.
pub fn install_fonts(ctx: &egui::Context) {
    let mut defs = FontDefinitions::default();
    let add = |defs: &mut FontDefinitions,
               name: &str,
               bytes: Vec<u8>,
               family: FontFamily,
               front: bool| {
        defs.font_data
            .insert(name.into(), Arc::new(FontData::from_owned(bytes)));
        let list = defs.families.entry(family).or_default();
        if front {
            list.insert(0, name.into())
        } else {
            list.push(name.into())
        }
    };
    if let Some(b) = system_font("Adwaita Sans", "Adwaita Sans") {
        add(&mut defs, "adwaita-sans", b, FontFamily::Proportional, true);
    }
    if let Some(b) = system_font(
        "JetBrainsMono Nerd Font Mono",
        "JetBrainsMono Nerd Font Mono",
    )
    .or_else(|| system_font("JetBrains Mono", "JetBrains Mono"))
    {
        add(&mut defs, "jetbrains-mono", b, FontFamily::Monospace, true);
    }
    let bold = system_font("Adwaita Sans:semibold", "Adwaita Sans");
    let bold_family = FontFamily::Name(BOLD.into());
    match bold {
        Some(b) => add(&mut defs, "adwaita-sans-bold", b, bold_family.clone(), true),
        None => {
            let fallback = defs.families[&FontFamily::Proportional].clone();
            defs.families.insert(bold_family.clone(), fallback);
        }
    }
    // Symbols that egui_commonmark and the UI use (🗐 ⏎ ⇧ ●) live here on most Linux systems.
    let symbols = system_font("Noto Sans Symbols 2", "Noto Sans Symbols 2");
    let phosphor = include_bytes!("../assets/fonts/Phosphor.ttf").to_vec();
    defs.font_data
        .insert("phosphor".into(), Arc::new(FontData::from_owned(phosphor)));
    let mut icons = vec!["phosphor".to_string()];
    icons.extend(defs.families[&FontFamily::Proportional].iter().cloned());
    defs.families.insert(FontFamily::Name(ICONS.into()), icons);
    if let Some(b) = symbols {
        defs.font_data
            .insert("symbols".into(), Arc::new(FontData::from_owned(b)));
    }
    for fam in [FontFamily::Proportional, FontFamily::Monospace, bold_family] {
        let list = defs.families.entry(fam).or_default();
        if defs.font_data.contains_key("symbols") {
            list.push("symbols".into());
        }
    }
    ctx.set_fonts(defs);
}

/// Visuals, spacing and text sizes for the palette.
pub fn apply(ctx: &egui::Context, theme: Theme, font_size: f32) {
    let p = palette(theme);
    let mut v = if p.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    v.panel_fill = p.bg_panel;
    v.window_fill = p.bg_raised;
    v.extreme_bg_color = p.bg_deep;
    v.faint_bg_color = p.bg_raised;
    v.code_bg_color = p.bg_deep;
    v.hyperlink_color = p.accent2;
    v.warn_fg_color = p.warn;
    v.error_fg_color = p.err;
    v.override_text_color = None;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(12);
    v.menu_corner_radius = CornerRadius::same(10);
    v.window_shadow = Shadow {
        offset: [0, 8],
        blur: 28,
        spread: 0,
        color: Color32::from_black_alpha(140),
    };
    v.popup_shadow = Shadow {
        offset: [0, 6],
        blur: 18,
        spread: 0,
        color: Color32::from_black_alpha(120),
    };
    v.selection.bg_fill = with_alpha(p.accent, 110);
    v.selection.stroke = Stroke::new(1.0, p.text);
    v.text_cursor.stroke = Stroke::new(2.0, p.accent2);
    let r = CornerRadius::same(7);
    v.widgets.noninteractive.bg_fill = p.bg_panel;
    v.widgets.noninteractive.weak_bg_fill = p.bg_panel;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text_dim);
    v.widgets.noninteractive.corner_radius = r;
    v.widgets.inactive.bg_fill = p.bg_raised;
    v.widgets.inactive.weak_bg_fill = p.bg_raised;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.inactive.corner_radius = r;
    v.widgets.hovered.bg_fill = p.bg_hover;
    v.widgets.hovered.weak_bg_fill = p.bg_hover;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, with_alpha(p.accent, 160));
    v.widgets.hovered.fg_stroke = Stroke::new(1.5, p.text);
    v.widgets.hovered.corner_radius = r;
    v.widgets.active.bg_fill = with_alpha(p.accent, 90);
    v.widgets.active.weak_bg_fill = with_alpha(p.accent, 90);
    v.widgets.active.bg_stroke = Stroke::new(1.0, p.accent);
    v.widgets.active.fg_stroke = Stroke::new(1.5, p.text);
    v.widgets.active.corner_radius = r;
    v.widgets.open.bg_fill = p.bg_hover;
    v.widgets.open.weak_bg_fill = p.bg_hover;
    v.widgets.open.bg_stroke = Stroke::new(1.0, p.accent);
    v.widgets.open.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.open.corner_radius = r;
    ctx.set_visuals(v);

    let size = font_size;
    BODY_SIZE.store(size.to_bits(), std::sync::atomic::Ordering::Relaxed);
    ctx.global_style_mut(|style| {
        style.text_styles = [
            (
                TextStyle::Small,
                FontId::new(size - 3.0, FontFamily::Proportional),
            ),
            (TextStyle::Body, FontId::new(size, FontFamily::Proportional)),
            (
                TextStyle::Button,
                FontId::new(size, FontFamily::Proportional),
            ),
            (
                TextStyle::Heading,
                FontId::new(size + 6.0, FontFamily::Name(BOLD.into())),
            ),
            (
                TextStyle::Monospace,
                FontId::new(size - 1.0, FontFamily::Monospace),
            ),
        ]
        .into();
        style.spacing.item_spacing = Vec2::new(8.0, 6.0);
        style.spacing.button_padding = Vec2::new(10.0, 5.0);
        style.spacing.menu_margin = Margin::same(8);
        style.spacing.interact_size.y = 26.0;
    });
}

/// The dock's look: tabs on a deep bar, panels as rounded cards, an accent outline on focus.
pub fn dock_style(style: &egui::Style, p: &Palette) -> egui_dock::Style {
    let mut d = egui_dock::Style::from_egui(style);
    d.dock_area_padding = Some(Margin::same(6));
    d.main_surface_border_stroke = Stroke::NONE;
    d.tab_bar.bg_fill = p.bg_deep;
    d.tab_bar.hline_color = p.border;
    d.tab_bar.height = 30.0;
    d.tab_bar.corner_radius = CornerRadius {
        nw: 10,
        ne: 10,
        sw: 0,
        se: 0,
    };
    d.tab.tab_body.bg_fill = p.bg_panel;
    d.tab.tab_body.stroke = Stroke::new(1.0, p.border);
    d.tab.tab_body.corner_radius = CornerRadius {
        nw: 0,
        ne: 0,
        sw: 10,
        se: 10,
    };
    d.tab.tab_body.inner_margin = Margin::same(10);
    d.tab.hline_below_active_tab_name = true;
    d.tab.active.bg_fill = p.bg_panel;
    d.tab.active.text_color = p.text;
    d.tab.active.outline_color = p.border;
    d.tab.focused.bg_fill = p.bg_panel;
    d.tab.focused.text_color = p.text;
    d.tab.focused.outline_color = with_alpha(p.accent, 200);
    d.tab.inactive.bg_fill = p.bg_deep;
    d.tab.inactive.text_color = p.text_faint;
    d.tab.inactive.outline_color = Color32::TRANSPARENT;
    d.tab.hovered.bg_fill = p.bg_raised;
    d.tab.hovered.text_color = p.text_dim;
    for s in [
        &mut d.tab.active,
        &mut d.tab.focused,
        &mut d.tab.inactive,
        &mut d.tab.hovered,
    ] {
        s.corner_radius = CornerRadius {
            nw: 8,
            ne: 8,
            sw: 0,
            se: 0,
        };
    }
    d.separator.width = 2.0;
    d.separator.color_idle = p.bg_deep;
    d.separator.color_hovered = p.accent;
    d.separator.color_dragged = p.accent2;
    d.buttons.close_tab_color = p.text_faint;
    d.buttons.close_tab_active_color = p.err;
    d.overlay.selection_color = with_alpha(p.accent, 60);
    d.overlay.button_color = p.accent;
    d
}

/// "void.ai" with a violet→cyan gradient, one glyph at a time.
pub fn wordmark(ui: &mut Ui, size: f32) -> Response {
    use egui::text::{LayoutJob, TextFormat};
    let p = palette_of(ui);
    let text = "void.ai";
    let n = text.chars().count().max(2) - 1;
    let mut job = LayoutJob::default();
    for (i, ch) in text.chars().enumerate() {
        job.append(
            &ch.to_string(),
            0.0,
            TextFormat {
                font_id: FontId::new(size, FontFamily::Name(BOLD.into())),
                color: lerp(p.accent, p.accent2, i as f32 / n as f32),
                ..Default::default()
            },
        );
    }
    ui.add(egui::Label::new(job).selectable(false))
}

/// The palette matching the UI's current dark/light mode.
pub fn palette_of(ui: &Ui) -> Palette {
    palette(if ui.visuals().dark_mode {
        Theme::Dark
    } else {
        Theme::Light
    })
}

/// A small rounded label with an icon, e.g. in the status bar.
pub fn pill(ui: &mut Ui, icon: &str, text: impl Into<String>, color: Color32) -> Response {
    let p = palette_of(ui);
    egui::Frame::new()
        .fill(with_alpha(color, if p.dark { 28 } else { 36 }))
        .stroke(Stroke::new(1.0, with_alpha(color, 70)))
        .corner_radius(CornerRadius::same(255))
        .inner_margin(Margin::symmetric(9, 2))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 5.0;
            ui.horizontal(|ui| {
                ui.label(ico(icon).color(color).size(small_size() + 1.0));
                ui.label(RichText::new(text.into()).color(p.text).small());
            });
        })
        .response
}

/// A thin rounded bar showing `fraction` full.
pub fn meter(ui: &mut Ui, width: f32, fraction: f32, color: Color32) -> Response {
    let p = palette_of(ui);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 6.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(3), p.bg_hover);
    let mut fill = rect;
    fill.set_width((rect.width() * fraction).max(if fraction > 0.0 { 3.0 } else { 0.0 }));
    painter.rect_filled(fill, CornerRadius::same(3), color);
    resp
}

/// A raised card for grouping settings.
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let p = palette_of(ui);
    egui::Frame::new()
        .fill(p.bg_raised)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// An icon-led section heading.
pub fn heading(ui: &mut Ui, icon: &str, text: &str) {
    let p = palette_of(ui);
    ui.horizontal(|ui| {
        ui.label(ico(icon).color(p.accent).size(18.0));
        ui.label(
            RichText::new(text)
                .family(FontFamily::Name(BOLD.into()))
                .size(16.0)
                .color(p.text),
        );
    });
}

/// A filled accent button with an icon.
pub fn primary_button(ui: &mut Ui, icon: &str, text: &str) -> Response {
    let p = palette_of(ui);
    let label = if text.is_empty() {
        ico(icon).color(Color32::WHITE).into()
    } else {
        itext_styled(icon, text, body_size(), Color32::WHITE)
    };
    ui.add(
        egui::Button::new(label)
            .fill(p.accent)
            .stroke(Stroke::new(1.0, lerp(p.accent, p.accent2, 0.35)))
            .corner_radius(CornerRadius::same(8)),
    )
}

/// A quiet button with an icon.
pub fn icon_button(ui: &mut Ui, icon: &str, hover: &str) -> Response {
    let p = palette_of(ui);
    ui.add(egui::Button::new(ico(icon).size(15.0).color(p.text_dim)).frame(false))
        .on_hover_text(hover)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lerp_hits_both_ends() {
        let a = Color32::from_rgb(0, 0, 0);
        let b = Color32::from_rgb(200, 100, 50);
        assert_eq!(lerp(a, b, 0.0), a);
        assert_eq!(lerp(a, b, 1.0), b);
        assert_eq!(lerp(a, b, 0.5), Color32::from_rgb(100, 50, 25));
    }

    #[test]
    fn meter_goes_green_amber_red() {
        let p = palette(Theme::Dark);
        assert_eq!(meter_color(&p, 0.1), p.ok);
        assert_eq!(meter_color(&p, 0.75), p.warn);
        assert_eq!(meter_color(&p, 0.95), p.err);
    }

    #[test]
    fn context_fraction_is_clamped() {
        assert_eq!(context_fraction(50, 100), 0.5);
        assert_eq!(context_fraction(500, 100), 1.0);
        assert_eq!(context_fraction(5, 0), 0.0);
    }

    #[test]
    fn palettes_keep_text_readable() {
        for t in [Theme::Dark, Theme::Light] {
            let p = palette(t);
            assert!(contrast(p.text, p.bg_panel) > 7.0, "{t:?} text contrast");
            assert!(
                contrast(p.text_dim, p.bg_panel) > 4.5,
                "{t:?} dim text contrast"
            );
        }
    }

    #[test]
    fn fc_match_output_is_checked_against_the_family() {
        assert_eq!(
            parse_fc_match("Adwaita Sans|/usr/share/fonts/a.ttf", "Adwaita Sans").as_deref(),
            Some("/usr/share/fonts/a.ttf")
        );
        // fc-match falls back to another family when the one asked for is missing.
        assert_eq!(
            parse_fc_match("DejaVu Sans|/usr/share/fonts/d.ttf", "Adwaita Sans"),
            None
        );
        assert_eq!(parse_fc_match("", "Adwaita Sans"), None);
    }
}
