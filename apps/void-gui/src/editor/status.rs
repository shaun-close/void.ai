//! The status line (mode, keys, messages, position, language, line endings, encoding) and
//! the `:` / `/` line.

use egui::{Color32, CornerRadius, FontId, Rect, RichText, Sense, Ui, UiBuilder, vec2};
use void_editor::LineEnding;
use void_vim::Mode;

use super::lang_name;
use super::state::{Doc, Prompt};
use crate::theme::{Palette, with_alpha};

fn mode_label(doc: &Doc, vim: bool) -> (&'static str, fn(&Palette) -> Color32) {
    if !vim {
        return ("EDIT", |p| p.text_dim);
    }
    match doc.mode() {
        Mode::Normal => ("NORMAL", |p| p.accent),
        Mode::Insert => ("INSERT", |p| p.ok),
        Mode::Replace => ("REPLACE", |p| p.err),
        Mode::Visual => ("VISUAL", |p| p.warn),
        Mode::VisualLine => ("V-LINE", |p| p.warn),
        Mode::VisualBlock => ("V-BLOCK", |p| p.warn),
    }
}

/// The editor: input (when focused), the text area, the `:` line and the status line.
/// `chat`: the tab shows a chat's version of the file (marked in the accent colour).
pub fn status_line(ui: &mut Ui, doc: &Doc, vim: bool, chat: bool, rect: Rect, pal: &Palette) {
    let mut ui = ui.new_child(UiBuilder::new().max_rect(rect));
    ui.horizontal_centered(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let (mode, color) = mode_label(doc, vim);
        let color = color(pal);
        egui::Frame::new()
            .fill(with_alpha(color, 40))
            .corner_radius(CornerRadius::same(4))
            .inner_margin(egui::Margin::symmetric(6, 0))
            .show(ui, |ui| {
                ui.label(RichText::new(mode).monospace().small().color(color));
            });
        if chat {
            ui.label(RichText::new("chat view").small().color(pal.accent2))
                .on_hover_text("Saving writes this chat's pending changes, not the file");
        }
        let st = doc.vim.state();
        if vim && !st.pending.is_empty() {
            ui.label(
                RichText::new(&st.pending)
                    .monospace()
                    .small()
                    .color(pal.text_dim),
            );
        }
        if let Some(r) = st.recording.filter(|_| vim) {
            ui.label(
                RichText::new(format!("recording @{r}"))
                    .small()
                    .color(pal.err),
            );
        }
        let engine_msg = (!st.message.is_empty()).then(|| {
            let err = st.message.starts_with('E')
                && st.message[1..].starts_with(|c: char| c.is_ascii_digit());
            (st.message.clone(), err)
        });
        if let Some((text, err)) = doc
            .core
            .message
            .as_ref()
            .map(|m| (m.text.clone(), m.error))
            .or(engine_msg)
        {
            ui.label(
                RichText::new(text)
                    .small()
                    .color(if err { pal.err } else { pal.text_dim }),
            );
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if !doc.highlighted() {
                ui.label(RichText::new("highlighting…").small().color(pal.text_faint));
            }
            let b = &doc.core.buffer;
            let enc = if b.read_only { "read-only" } else { "UTF-8" };
            ui.label(RichText::new(enc).small().color(if b.read_only {
                pal.warn
            } else {
                pal.text_faint
            }));
            let eol = match b.line_ending {
                LineEnding::Lf => "LF",
                LineEnding::CrLf => "CRLF",
            };
            ui.label(RichText::new(eol).small().color(pal.text_faint));
            ui.label(
                RichText::new(lang_name(doc.lang()))
                    .small()
                    .color(pal.text_dim),
            );
            let (l, c) = doc.core.line_col();
            ui.label(
                RichText::new(format!("{}:{}", l + 1, c + 1))
                    .small()
                    .color(pal.text_dim),
            );
        });
    });
}

/// The line being typed after `:` / `/`, with a block cursor at its end.
pub fn prompt_line(
    ui: &mut Ui,
    pr: &Prompt,
    r: Rect,
    font: &FontId,
    char_w: f32,
    row_h: f32,
    pal: &Palette,
) {
    ui.painter()
        .rect_filled(r, CornerRadius::same(4), pal.bg_deep);
    let mut child = ui.new_child(UiBuilder::new().max_rect(r.shrink2(vec2(6.0, 2.0))));
    child.horizontal_centered(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.label(
            RichText::new(format!("{}{}", pr.prefix, pr.text))
                .font(font.clone())
                .color(pal.text),
        );
        let (cr, _) = ui.allocate_exact_size(vec2(char_w, row_h - 4.0), Sense::hover());
        ui.painter()
            .rect_filled(cr, CornerRadius::ZERO, with_alpha(pal.accent2, 170));
    });
}
