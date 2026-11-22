//! The Chat panel: one conversation's messages, the streaming reply, and the input card.

use egui::{
    Align, Color32, CornerRadius, FontFamily, Id, Key, Layout, Margin, Modifiers, RichText, Sense,
    Stroke, TextEdit, Ui, Vec2,
};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use void_core::engine::Command;
use void_core::model::{ConversationId, Message, Role};

use super::{Action, Panels};
use crate::theme::{self, BOLD, Palette, icon, lerp, with_alpha};

const TITLE_MAX: usize = 30;

/// A Chat tab's title: the conversation's title, cut to 30 characters.
pub fn tab_title(title: Option<&str>) -> String {
    match title {
        None => "Chat".into(),
        Some(t) if t.chars().count() > TITLE_MAX => {
            let mut s: String = t.chars().take(TITLE_MAX - 1).collect();
            s.push('…');
            s
        }
        Some(t) => t.into(),
    }
}

/// Prompts offered on an empty chat.
pub const SUGGESTIONS: [(&str, &str); 3] = [
    (icon::LIGHTNING, "Explain this Rust borrow-checker error"),
    (icon::GAUGE, "Write a moon task that runs cargo nextest"),
    (icon::BRAIN, "Review my design for a streaming SSE parser"),
];

fn markdown(
    ui: &mut Ui,
    md: &mut CommonMarkCache,
    id: impl std::hash::Hash + std::fmt::Debug,
    text: &str,
) {
    CommonMarkViewer::new().show_with_id(Id::unique(id), ui, md, text);
}

/// The round gradient avatar in front of the assistant's replies.
fn avatar(ui: &mut Ui, p: &Palette, live: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::hover());
    let painter = ui.painter();
    let t = if live {
        ((ui.input(|i| i.time) * 2.0).sin() as f32 + 1.0) / 2.0
    } else {
        0.0
    };
    painter.circle_filled(
        rect.center(),
        14.0,
        lerp(p.accent, p.accent2, 0.25 + 0.5 * t),
    );
    painter.circle_stroke(
        rect.center(),
        14.0,
        Stroke::new(1.5, with_alpha(p.accent2, 140)),
    );
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        icon::SPARKLE,
        egui::FontId::new(15.0, FontFamily::Name(theme::ICONS.into())),
        Color32::WHITE,
    );
}

/// Three dots that pulse while a reply is arriving.
fn typing_dots(ui: &mut Ui, p: &Palette) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(36.0, 14.0), Sense::hover());
    let time = ui.input(|i| i.time);
    for i in 0..3 {
        let phase = ((time * 4.0 - i as f64 * 0.6).sin() as f32 + 1.0) / 2.0;
        let c = lerp(with_alpha(p.accent, 60), p.accent2, phase);
        let x = rect.left() + 6.0 + i as f32 * 12.0;
        ui.painter()
            .circle_filled(egui::pos2(x, rect.center().y), 3.0 + phase, c);
    }
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(33));
}

fn reasoning_card(
    ui: &mut Ui,
    p: &Palette,
    salt: impl std::hash::Hash + std::fmt::Debug,
    text: &str,
    live: bool,
    open: bool,
) {
    if text.is_empty() {
        return;
    }
    let words = text.split_whitespace().count();
    let title = if live {
        "Thinking…".to_string()
    } else {
        format!("Thought · {words} words")
    };
    egui::Frame::new()
        .fill(with_alpha(p.accent2, if p.dark { 10 } else { 18 }))
        .stroke(Stroke::new(1.0, with_alpha(p.accent2, 45)))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
            egui::CollapsingHeader::new(theme::itext_styled(
                icon::BRAIN,
                &title,
                theme::small_size(),
                p.accent2,
            ))
            .id_salt(salt)
            .default_open(open)
            .show(ui, |ui| {
                ui.add(
                    egui::Label::new(RichText::new(text).italics().color(p.text_dim))
                        .selectable(true)
                        .wrap(),
                );
            });
        });
}

fn user_bubble(
    ui: &mut Ui,
    p: &Palette,
    md: &mut CommonMarkCache,
    id: impl std::hash::Hash + std::fmt::Debug,
    text: &str,
) {
    let max = ui.available_width() * 0.85;
    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
        egui::Frame::new()
            .fill(lerp(
                p.bg_raised,
                p.accent,
                if p.dark { 0.22 } else { 0.12 },
            ))
            .stroke(Stroke::new(1.0, with_alpha(p.accent, 110)))
            .corner_radius(CornerRadius {
                nw: 14,
                ne: 4,
                sw: 14,
                se: 14,
            })
            .inner_margin(Margin::symmetric(14, 10))
            .show(ui, |ui| {
                ui.set_max_width(max);
                ui.with_layout(Layout::top_down(Align::Min), |ui| {
                    markdown(ui, md, id, text);
                });
            });
    });
}

/// Avatar, "void", the model and the token count, on one row.
fn reply_header(ui: &mut Ui, p: &Palette, model: &str, tokens: Option<u32>, live: bool) {
    ui.horizontal(|ui| {
        avatar(ui, p, live);
        ui.label(
            RichText::new("void")
                .family(FontFamily::Name(BOLD.into()))
                .color(p.text),
        );
        // Truncate rather than widen the panel: a long model name would stop replies wrapping.
        let meta = match tokens {
            Some(t) => format!("{model} · {t} tokens"),
            None => model.to_string(),
        };
        ui.add(egui::Label::new(RichText::new(meta).small().color(p.text_faint)).truncate());
    });
}

/// The reply itself, indented under the name so it lines up, at the panel's full width.
fn reply_body(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .inner_margin(Margin {
            left: 36,
            right: 0,
            top: 0,
            bottom: 0,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

#[allow(clippy::too_many_arguments)]
fn assistant_block(
    ui: &mut Ui,
    p: &Palette,
    md: &mut CommonMarkCache,
    conv: ConversationId,
    index: usize,
    m: &Message,
    model: &str,
    retry: bool,
    engine: &void_core::engine::EngineHandle,
) {
    reply_header(ui, p, model, m.usage.map(|u| u.completion_tokens), false);
    reply_body(ui, |ui| {
        let text = m.text();
        reasoning_card(
            ui,
            p,
            (conv, index, "r"),
            &m.reasoning(),
            false,
            text.is_empty(),
        );
        if !text.is_empty() {
            markdown(ui, md, (conv, index), &text);
        }
        if let Some(err) = &m.error {
            egui::Frame::new()
                .fill(with_alpha(p.err, 22))
                .stroke(Stroke::new(1.0, with_alpha(p.err, 90)))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::symmetric(10, 6))
                .show(ui, |ui| {
                    ui.label(theme::itext_styled(
                        icon::WARNING,
                        err,
                        theme::body_size(),
                        p.err,
                    ));
                });
        }
        ui.horizontal(|ui| {
            if m.stopped {
                ui.label(theme::itext_styled(
                    icon::STOP,
                    "stopped",
                    theme::small_size(),
                    p.warn,
                ));
            }
            if !text.is_empty() && theme::icon_button(ui, icon::COPY, "Copy reply").clicked() {
                ui.ctx().copy_text(text.clone());
            }
            if retry
                && ui
                    .button(theme::itext_small(icon::ARROW_CLOCKWISE, "Retry"))
                    .clicked()
            {
                engine.send(Command::Retry(conv));
            }
        });
    });
}

fn empty_state(ui: &mut Ui, p: &Palette, input: &mut String) {
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.22).max(16.0));
        theme::wordmark(ui, 44.0);
        ui.add_space(2.0);
        ui.label(RichText::new("your local AI workbench").color(p.text_dim));
        ui.add_space(18.0);
        for (ic, text) in SUGGESTIONS {
            let b = egui::Button::new(theme::itext_styled(
                ic,
                text,
                theme::body_size(),
                p.text_dim,
            ))
            .fill(p.bg_raised)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(CornerRadius::same(10))
            .wrap()
            .min_size(Vec2::new(ui.available_width().min(360.0), 34.0));
            if ui.add(b).clicked() {
                *input = text.to_string();
            }
            ui.add_space(4.0);
        }
    });
}

/// Draw a Chat tab. `tab` is its id; `conv` the conversation it shows (None until the first send).
pub fn show(ui: &mut Ui, p: &mut Panels, tab: u32, conv: Option<ConversationId>) {
    let pal = theme::palette_of(ui);
    let streaming = conv.is_some_and(|c| p.state.streams.contains_key(&c));

    egui::Panel::bottom(("chat-input", tab))
        .frame(egui::Frame::new().inner_margin(Margin {
            left: 0,
            right: 0,
            top: 8,
            bottom: 2,
        }))
        .show(ui, |ui| input_card(ui, &pal, p, tab, conv, streaming));

    let Some(c) = conv else {
        let buf = p.inputs.entry(tab).or_default();
        empty_state(ui, &pal, buf);
        return;
    };
    let Some(loaded) = p.state.loaded.get(&c) else {
        ui.centered_and_justified(|ui| ui.spinner());
        return;
    };
    let model = p
        .state
        .profile_for(Some(c))
        .map(|pr| pr.model.clone())
        .unwrap_or_default();
    egui::ScrollArea::vertical()
        .id_salt(("chat-scroll", tab))
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(4.0);
            let n = loaded.messages.len();
            for (i, m) in loaded.messages.iter().enumerate() {
                if m.role == Role::User {
                    user_bubble(ui, &pal, p.md, (c, i), &m.text());
                } else {
                    let last = i + 1 == n && !streaming;
                    let retry = last && (m.error.is_some() || m.stopped);
                    assistant_block(ui, &pal, p.md, c, i, m, &model, retry, p.engine);
                }
                ui.add_space(12.0);
            }
            if let Some(s) = p.state.streams.get(&c) {
                reply_header(ui, &pal, &model, None, true);
                reply_body(ui, |ui| {
                    reasoning_card(
                        ui,
                        &pal,
                        (c, "live"),
                        &s.reasoning,
                        s.text.is_empty(),
                        false,
                    );
                    if !s.text.is_empty() {
                        markdown(ui, p.md, (c, "live-text"), &s.text);
                    }
                    typing_dots(ui, &pal);
                });
                ui.add_space(8.0);
            }
        });
}

fn input_card(
    ui: &mut Ui,
    pal: &Palette,
    p: &mut Panels,
    tab: u32,
    conv: Option<ConversationId>,
    streaming: bool,
) {
    let input_id = Id::unique(("chat-input-edit", tab));
    let focused = ui.memory(|m| m.has_focus(input_id));
    let border = if focused {
        with_alpha(pal.accent, 200)
    } else {
        pal.border
    };
    egui::Frame::new()
        .fill(pal.bg_raised)
        .stroke(Stroke::new(if focused { 1.5 } else { 1.0 }, border))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let buf = p.inputs.entry(tab).or_default();
            let mut send = focused && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
            ui.add(
                TextEdit::multiline(buf)
                    .id(input_id)
                    .frame(egui::Frame::NONE)
                    .desired_rows(2)
                    .desired_width(f32::INFINITY)
                    .hint_text(RichText::new("Ask void anything…").color(pal.text_faint)),
            );
            ui.horizontal_wrapped(|ui| {
                if let Some(c) = conv
                    && let Some(loaded) = p.state.loaded.get(&c)
                {
                    let current = loaded.profile_id.clone();
                    let mut chosen = current.clone();
                    let name = |id: &Option<String>| match id {
                        None => p
                            .state
                            .saved_config
                            .profile(&p.state.saved_config.active_profile)
                            .map(|x| format!("{} (default)", x.name))
                            .unwrap_or_else(|| "default".into()),
                        Some(id) => p
                            .state
                            .saved_config
                            .profile(id)
                            .map(|x| x.name.clone())
                            .unwrap_or_else(|| format!("{id} (missing)")),
                    };
                    egui::ComboBox::from_id_salt(("chat-profile", tab))
                        .selected_text(theme::itext_small(icon::PLUGS, &name(&current)))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut chosen, None, name(&None));
                            for prof in &p.state.saved_config.profiles {
                                ui.selectable_value(&mut chosen, Some(prof.id.clone()), &prof.name);
                            }
                        });
                    if chosen != current {
                        p.engine.send(Command::SetConversationProfile(c, chosen));
                    }
                }
                if ui.available_width() > 260.0 {
                    ui.label(
                        RichText::new("⏎ send · ⇧⏎ new line")
                            .small()
                            .color(pal.text_faint),
                    );
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if streaming {
                        let stop = egui::Button::new(theme::itext_styled(
                            icon::STOP,
                            "Stop",
                            theme::body_size(),
                            Color32::WHITE,
                        ))
                        .fill(pal.err)
                        .corner_radius(CornerRadius::same(8));
                        if ui.add(stop).clicked()
                            && let Some(c) = conv
                        {
                            p.engine.send(Command::Stop(c));
                        }
                    } else if theme::primary_button(ui, icon::SEND, "Send").clicked() {
                        send = true;
                    }
                });
            });
            let buf = p.inputs.entry(tab).or_default();
            if send && !streaming && !buf.trim().is_empty() {
                let text = std::mem::take(buf).trim_end().to_string();
                match conv {
                    Some(c) => p.engine.send(Command::SendMessage {
                        conversation: c,
                        text,
                    }),
                    None => p.actions.push(Action::StartChat { tab, text }),
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_title_truncates() {
        assert_eq!(tab_title(None), "Chat");
        assert_eq!(tab_title(Some("short")), "short");
        let t = tab_title(Some(&"y".repeat(40)));
        assert_eq!(t.chars().count(), 30);
        assert!(t.ends_with('…'));
    }
}
