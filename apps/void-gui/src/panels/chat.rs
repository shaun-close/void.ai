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

/// What a tool card shows.
#[derive(Clone, Debug, PartialEq)]
pub struct CardInfo {
    pub name: String,
    pub summary: String,
    pub output: String,
    pub status: CardStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardStatus {
    Running,
    Done,
    Failed,
}

pub fn tool_icon(name: &str) -> &'static str {
    match name {
        "read" => icon::FILE_TEXT,
        "list" | "glob" | "grep" => icon::MAGNIFIER,
        "edit" | "write" => icon::PENCIL,
        "bash" => icon::TERMINAL,
        _ => icon::ROBOT,
    }
}

/// A one-line summary from raw tool arguments (when the live card isn't around).
pub fn args_summary(arguments: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
    ["command", "path", "pattern"]
        .iter()
        .find_map(|k| v.get(*k).and_then(|x| x.as_str()))
        .unwrap_or("")
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(80)
        .collect()
}

/// Cards for a reply's tool calls: live state when the call ran this session, otherwise the
/// stored result.
fn cards_for(
    state: &crate::state::UiState,
    conv: &void_core::model::Conversation,
    m: &Message,
) -> Vec<CardInfo> {
    m.parts
        .iter()
        .filter_map(|part| match part {
            void_core::model::Part::ToolCall {
                id,
                name,
                arguments,
            } => Some((id, name, arguments)),
            _ => None,
        })
        .map(|(id, name, arguments)| {
            if let Some(live) = state.tool_calls.get(id) {
                let (output, status) = match &live.done {
                    Some(o) => (
                        o.content.clone(),
                        if o.is_error {
                            CardStatus::Failed
                        } else {
                            CardStatus::Done
                        },
                    ),
                    None => (live.output.clone(), CardStatus::Running),
                };
                return CardInfo {
                    name: name.clone(),
                    summary: live.summary.clone(),
                    output,
                    status,
                };
            }
            let stored = conv
                .messages
                .iter()
                .flat_map(|m| m.parts.iter())
                .find_map(|p| match p {
                    void_core::model::Part::ToolResult { call_id, content } if call_id == id => {
                        Some(content.clone())
                    }
                    _ => None,
                });
            CardInfo {
                name: name.clone(),
                summary: args_summary(arguments),
                status: if stored.is_some() {
                    CardStatus::Done
                } else {
                    CardStatus::Failed
                },
                output: stored.unwrap_or_else(|| "(no result)".into()),
            }
        })
        .collect()
}

fn tool_card(
    ui: &mut Ui,
    p: &Palette,
    salt: impl std::hash::Hash + std::fmt::Debug,
    card: &CardInfo,
) {
    egui::Frame::new()
        .fill(p.bg_raised)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
            ui.horizontal(|ui| {
                ui.label(theme::ico(tool_icon(&card.name)).color(p.accent2));
                ui.label(
                    RichText::new(&card.name)
                        .family(FontFamily::Name(BOLD.into()))
                        .color(p.text),
                );
                match card.status {
                    CardStatus::Running => {
                        ui.spinner();
                    }
                    CardStatus::Done => {
                        ui.label(theme::ico(icon::CHECK_CIRCLE).color(p.ok));
                    }
                    CardStatus::Failed => {
                        ui.label(theme::ico(icon::X_CIRCLE).color(p.err));
                    }
                }
                ui.add(
                    egui::Label::new(RichText::new(&card.summary).monospace().color(p.text_dim))
                        .truncate(),
                );
            });
            if !card.output.is_empty() {
                egui::CollapsingHeader::new(RichText::new("output").small().color(p.text_faint))
                    .id_salt(salt)
                    .default_open(card.status == CardStatus::Running && card.name == "bash")
                    .show(ui, |ui| {
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                        let shown = tail(&card.output, 6000);
                        ui.add(
                            egui::Label::new(RichText::new(shown).monospace().small())
                                .selectable(true),
                        );
                    });
            }
        });
}

fn tail(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut i = s.len() - max;
    while !s.is_char_boundary(i) {
        i += 1;
    }
    &s[i..]
}

fn approval_card(
    ui: &mut Ui,
    p: &Palette,
    req: &void_core::permissions::ApprovalRequest,
    engine: &void_core::engine::EngineHandle,
) {
    egui::Frame::new()
        .fill(with_alpha(p.warn, if p.dark { 18 } else { 28 }))
        .stroke(Stroke::new(1.0, with_alpha(p.warn, 120)))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(theme::ico(icon::HAND).color(p.warn).size(18.0));
                ui.label(
                    RichText::new(format!("void wants to use {}", req.tool))
                        .family(FontFamily::Name(BOLD.into()))
                        .color(p.text),
                );
            });
            let what = req.command.clone().unwrap_or_else(|| req.summary.clone());
            egui::Frame::new()
                .fill(p.bg_deep)
                .corner_radius(CornerRadius::same(6))
                .inner_margin(Margin::symmetric(8, 6))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.add(
                        egui::Label::new(RichText::new(&what).monospace())
                            .selectable(true)
                            .wrap(),
                    );
                });
            ui.horizontal_wrapped(|ui| {
                use void_core::engine::Command;
                use void_core::permissions::Decision;
                if theme::primary_button(ui, icon::CHECK, "Allow once").clicked() {
                    engine.send(Command::Approve {
                        id: req.id,
                        decision: Decision::Once,
                    });
                }
                let prefix = req
                    .command
                    .as_deref()
                    .map(void_core::permissions::always_prefix)
                    .unwrap_or_default();
                let always = if prefix.is_empty() {
                    format!("Always allow {}", req.tool)
                } else {
                    format!("Always allow `{prefix}`")
                };
                if ui
                    .button(theme::itext(icon::SHIELD_CHECK, &always))
                    .clicked()
                {
                    engine.send(Command::Approve {
                        id: req.id,
                        decision: Decision::AlwaysForWorkspace(prefix),
                    });
                }
                if ui
                    .button(theme::itext_styled(
                        icon::PROHIBIT,
                        "Deny",
                        theme::body_size(),
                        p.err,
                    ))
                    .clicked()
                {
                    engine.send(Command::Approve {
                        id: req.id,
                        decision: Decision::Deny,
                    });
                }
            });
        });
}

fn summary_divider(
    ui: &mut Ui,
    p: &Palette,
    salt: impl std::hash::Hash + std::fmt::Debug,
    text: &str,
) {
    ui.horizontal(|ui| {
        ui.label(theme::ico(icon::LAYOUT).color(p.text_faint));
        egui::CollapsingHeader::new(
            RichText::new("Earlier messages summarised")
                .small()
                .color(p.text_faint),
        )
        .id_salt(salt)
        .show(ui, |ui| {
            ui.add(egui::Label::new(RichText::new(text).italics().color(p.text_dim)).wrap());
        });
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
    cards: &[CardInfo],
    header: bool,
) {
    if header {
        reply_header(ui, p, model, m.usage.map(|u| u.completion_tokens), false);
    }
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
        for (k, card) in cards.iter().enumerate() {
            tool_card(ui, p, (conv, index, k, "tool"), card);
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
    // Replying or running tools: Stop instead of Send.
    let streaming = conv.is_some_and(|c| p.state.busy(c));

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
                // Tool results show inside their call's card.
                if m.role == Role::Tool {
                    continue;
                }
                let summary: String = m
                    .parts
                    .iter()
                    .filter_map(|x| match x {
                        void_core::model::Part::Summary(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect();
                if !summary.is_empty() {
                    summary_divider(ui, &pal, (c, i, "summary"), &summary);
                    ui.add_space(8.0);
                    continue;
                }
                if m.role == Role::User {
                    user_bubble(ui, &pal, p.md, (c, i), &m.text());
                } else {
                    let last = i + 1 == n && !streaming;
                    let retry = last && (m.error.is_some() || m.stopped);
                    let cards = cards_for(p.state, loaded, m);
                    // Consecutive tool rounds read as one reply: one header.
                    let header = !(i > 0 && loaded.messages[i - 1].role == Role::Tool);
                    assistant_block(
                        ui, &pal, p.md, c, i, m, &model, retry, p.engine, &cards, header,
                    );
                }
                ui.add_space(
                    if m.parts
                        .iter()
                        .any(|x| matches!(x, void_core::model::Part::ToolCall { .. }))
                    {
                        4.0
                    } else {
                        12.0
                    },
                );
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
            for req in p.state.approvals.iter().filter(|a| a.conversation == c) {
                approval_card(ui, &pal, req, p.engine);
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
                if let Some(c) = conv
                    && let Some(loaded) = p.state.loaded.get(&c)
                {
                    if p.state.workspace.root.is_some() {
                        let current = loaded.agent.clone();
                        let mut chosen = current.clone();
                        let agents = p.state.saved_config.agents.clone();
                        let default = p.state.saved_config.default_agent.clone();
                        let name = |id: &Option<String>| {
                            let id = id.clone().unwrap_or_else(|| default.clone());
                            agents
                                .iter()
                                .find(|a| a.id == id)
                                .map(|a| a.name.clone())
                                .unwrap_or(id)
                        };
                        egui::ComboBox::from_id_salt(("chat-agent", tab))
                            .selected_text(theme::itext_small(icon::ROBOT, &name(&current)))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut chosen,
                                    None,
                                    format!("{} (default)", name(&None)),
                                );
                                for a in &agents {
                                    ui.selectable_value(&mut chosen, Some(a.id.clone()), &a.name);
                                }
                            });
                        if chosen != current {
                            p.engine.send(Command::SetConversationAgent(c, chosen));
                        }
                    } else if ui.available_width() > 360.0 {
                        ui.label(
                            RichText::new("Open a folder to let void use tools")
                                .small()
                                .color(pal.text_faint),
                        );
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
