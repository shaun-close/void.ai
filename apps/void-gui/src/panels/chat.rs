//! The Chat panel: one conversation's messages, the streaming reply, and the input card.

use egui::{
    Align, Color32, CornerRadius, FontFamily, Id, Key, Layout, Margin, Modifiers, RichText, Sense,
    Stroke, TextEdit, Ui, Vec2,
};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use void_core::engine::Command;
use void_core::model::{ConversationId, Message, Role};

use super::{Action, Panels, claude_cards};
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

/// Avatar, who's replying (void, or Claude Code), the model and the token count, on one row.
fn reply_header(
    ui: &mut Ui,
    p: &Palette,
    (who, model): (&str, &str),
    tokens: Option<u32>,
    live: bool,
) {
    ui.horizontal(|ui| {
        avatar(ui, p, live);
        ui.label(
            RichText::new(who)
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
    /// The call's arguments (JSON); empty while only the live summary is known.
    pub arguments: String,
    pub summary: String,
    pub output: String,
    /// What streamed onto the card as it ran (a subagent's work, for Claude Code's Task).
    pub live: String,
    pub status: CardStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardStatus {
    Running,
    Done,
    Failed,
}

/// The card's icon, for void's tools and Claude Code's alike.
pub fn tool_icon(name: &str) -> &'static str {
    if name.starts_with("mcp__") {
        return icon::PLUGS;
    }
    match name.to_ascii_lowercase().as_str() {
        "read" => icon::FILE_TEXT,
        "list" | "glob" | "grep" => icon::MAGNIFIER,
        "edit" | "write" | "multiedit" | "notebookedit" => icon::PENCIL,
        "bash" => icon::TERMINAL,
        "webfetch" | "websearch" => icon::ARROW_SQUARE_OUT,
        "todowrite" => icon::CHECK_CIRCLE,
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

fn live_card(name: &str, arguments: &str, live: &crate::state::ToolCallView) -> CardInfo {
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
    CardInfo {
        name: name.to_string(),
        arguments: arguments.to_string(),
        summary: live.summary.clone(),
        output,
        live: live.output.clone(),
        status,
    }
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
                return live_card(name, arguments, live);
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
                arguments: arguments.clone(),
                summary: args_summary(arguments),
                status: if stored.is_some() {
                    CardStatus::Done
                } else {
                    CardStatus::Failed
                },
                output: stored.unwrap_or_else(|| "(no result)".into()),
                live: String::new(),
            }
        })
        .collect()
}

/// Cards for a chat's running tool calls that no stored message holds yet: Claude Code's reply
/// is stored once its tool results arrive, so a running Bash or subagent shows here meanwhile.
fn pending_cards(
    state: &crate::state::UiState,
    conv: &void_core::model::Conversation,
) -> Vec<CardInfo> {
    let stored = |id: &str| {
        conv.messages
            .iter()
            .flat_map(|m| m.parts.iter())
            .any(|p| matches!(p, void_core::model::Part::ToolCall { id: x, .. } if x == id))
    };
    let mut running: Vec<_> = state
        .tool_calls
        .iter()
        .filter(|(id, t)| t.conversation == conv.id && t.done.is_none() && !stored(id))
        .collect();
    running.sort_by_key(|(_, t)| t.order);
    running
        .into_iter()
        .map(|(_, t)| live_card(&t.name, "", t))
        .collect()
}

/// The tool cards of one assistant message.
pub(crate) fn tool_cards(
    ui: &mut Ui,
    p: &Palette,
    state: &crate::state::UiState,
    conv: &void_core::model::Conversation,
    m: &Message,
    index: usize,
) {
    let root = state.workspace.root.as_deref();
    for (k, card) in cards_for(state, conv, m).iter().enumerate() {
        tool_card(ui, p, (conv.id, index, k, "tool"), card, root);
    }
}

/// A short diff of an edit, like Review's lines.
fn edit_preview(ui: &mut Ui, p: &Palette, lines: &[(char, String)]) {
    if lines.is_empty() {
        return;
    }
    egui::Frame::new()
        .fill(p.bg_deep)
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            for (c, line) in lines.iter().take(claude_cards::PREVIEW_LINES) {
                let bg = match c {
                    '+' => with_alpha(p.ok, 22),
                    '-' => with_alpha(p.err, 22),
                    _ => Color32::TRANSPARENT,
                };
                egui::Frame::new().fill(bg).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!("{c} {line}"))
                                .family(FontFamily::Monospace)
                                .size(theme::small_size())
                                .color(super::review::diff_line_color(p, *c)),
                        )
                        .truncate(),
                    );
                });
            }
            let more = lines.len().saturating_sub(claude_cards::PREVIEW_LINES);
            if more > 0 {
                ui.label(
                    RichText::new(format!("… {more} more lines"))
                        .small()
                        .color(p.text_faint),
                );
            }
        });
}

/// Monospace text behind a collapsed header.
fn collapsed_text(
    ui: &mut Ui,
    p: &Palette,
    salt: impl std::hash::Hash + std::fmt::Debug,
    title: &str,
    text: &str,
    open: bool,
) {
    egui::CollapsingHeader::new(RichText::new(title).small().color(p.text_faint))
        .id_salt(salt)
        .default_open(open)
        .show(ui, |ui| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
            ui.add(
                egui::Label::new(RichText::new(tail(text, 6000)).monospace().small())
                    .selectable(true),
            );
        });
}

fn tool_card(
    ui: &mut Ui,
    p: &Palette,
    salt: impl std::hash::Hash + std::fmt::Debug + Copy,
    card: &CardInfo,
    root: Option<&std::path::Path>,
) {
    let claude = claude_cards::is_claude_tool(&card.name);
    let (title, summary) = if claude {
        (
            claude_cards::display_name(&card.name),
            claude_cards::summary(&card.name, &card.arguments, &card.summary, root),
        )
    } else {
        (card.name.clone(), card.summary.clone())
    };
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
                    RichText::new(&title)
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
                    egui::Label::new(RichText::new(&summary).monospace().color(p.text_dim))
                        .truncate(),
                );
            });
            let running = card.status == CardStatus::Running;
            if claude && claude_cards::is_edit(&card.name) {
                edit_preview(
                    ui,
                    p,
                    &claude_cards::edit_preview(&card.name, &card.arguments),
                );
            }
            if claude && claude_cards::shows_arguments(&card.name) && !card.arguments.is_empty() {
                collapsed_text(
                    ui,
                    p,
                    (salt, "args"),
                    "arguments",
                    &claude_cards::pretty_arguments(&card.arguments),
                    false,
                );
            }
            if claude && claude_cards::is_subagent(&card.name) {
                // The subagent's own calls and text: nested here, collapsed, never as cards.
                if !card.live.is_empty() {
                    collapsed_text(
                        ui,
                        p,
                        (salt, "subagent"),
                        &claude_cards::subagent_header(&card.live),
                        &card.live,
                        false,
                    );
                }
                if !running && !card.output.is_empty() {
                    collapsed_text(ui, p, (salt, "result"), "result", &card.output, false);
                }
                return;
            }
            if !card.output.is_empty() {
                let bash = card.name.eq_ignore_ascii_case("bash");
                collapsed_text(ui, p, salt, "output", &card.output, running && bash);
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

/// A request waiting on the user. Claude Code's say so and show its own wording; "Always
/// allow" is offered for them only when Claude Code suggested a rule, and hands that back
/// ([`Decision::AlwaysAsSuggested`]). The chosen decision goes to `send`.
fn approval_card(
    ui: &mut Ui,
    p: &Palette,
    req: &void_core::permissions::ApprovalRequest,
    send: &mut dyn FnMut(Command),
) {
    use void_core::permissions::{ApprovalKind, Decision};
    let (who, tool) = match &req.kind {
        ApprovalKind::ClaudeCode { .. } => ("Claude Code", claude_cards::display_name(&req.tool)),
        ApprovalKind::Void => ("void", req.tool.clone()),
    };
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
                    RichText::new(format!("{who} wants to use {tool}"))
                        .family(FontFamily::Name(BOLD.into()))
                        .color(p.text),
                );
            });
            if let ApprovalKind::ClaudeCode {
                title, description, ..
            } = &req.kind
            {
                if let Some(t) = title.as_deref().filter(|t| !t.trim().is_empty()) {
                    ui.add(egui::Label::new(RichText::new(t).color(p.text)).wrap());
                }
                if let Some(d) = description.as_deref().filter(|d| !d.trim().is_empty()) {
                    ui.add(egui::Label::new(RichText::new(d).color(p.text_dim)).wrap());
                }
            }
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
            if let ApprovalKind::ClaudeCode {
                blocked_path: Some(path),
                ..
            } = &req.kind
            {
                ui.label(theme::itext_styled(
                    icon::SHIELD_WARNING,
                    &format!("Outside what Claude Code may touch: {path}"),
                    theme::small_size(),
                    p.warn,
                ));
            }
            ui.horizontal_wrapped(|ui| {
                let mut decide = |d: Decision| {
                    send(Command::Approve {
                        id: req.id,
                        decision: d,
                    })
                };
                if theme::primary_button(ui, icon::CHECK, "Allow once").clicked() {
                    decide(Decision::Once);
                }
                // Claude Code's requests: "Always allow" only when Claude Code suggested a
                // permission update, which goes back to it (void's rules are never written).
                let always = match &req.kind {
                    ApprovalKind::ClaudeCode { always_allow, .. } => always_allow
                        .then(|| ("Always allow".to_string(), Decision::AlwaysAsSuggested)),
                    ApprovalKind::Void => {
                        let prefix = req
                            .command
                            .as_deref()
                            .map(void_core::permissions::always_prefix)
                            .unwrap_or_default();
                        let label = if prefix.is_empty() {
                            format!("Always allow {}", req.tool)
                        } else {
                            format!("Always allow `{prefix}`")
                        };
                        Some((label, Decision::AlwaysForWorkspace(prefix)))
                    }
                };
                if let Some((label, decision)) = always
                    && ui
                        .button(theme::itext(icon::SHIELD_CHECK, &label))
                        .clicked()
                {
                    decide(decision);
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
                    decide(Decision::Deny);
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
    who: (&str, &str),
    retry: bool,
    engine: &void_core::engine::EngineHandle,
    state: &crate::state::UiState,
    loaded: &void_core::model::Conversation,
    header: bool,
) {
    if header {
        reply_header(ui, p, who, m.usage.map(|u| u.completion_tokens), false);
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
        tool_cards(ui, p, state, loaded, m, index);
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
    let profile = p.state.profile_for(Some(c));
    let claude = profile.is_some_and(|pr| pr.kind == void_core::config::BackendKind::ClaudeCode);
    let model = match profile {
        // The model Claude Code said it's using, else the configured one, else its default.
        Some(pr) if claude => p
            .state
            .claude_models
            .get(&c)
            .filter(|m| !m.is_empty())
            .unwrap_or(&pr.model)
            .clone(),
        Some(pr) => pr.model.clone(),
        None => String::new(),
    };
    let who = (if claude { "Claude Code" } else { "void" }, model.as_str());
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
                    // Consecutive tool rounds read as one reply: one header.
                    let header = !(i > 0 && loaded.messages[i - 1].role == Role::Tool);
                    assistant_block(
                        ui, &pal, p.md, c, i, m, who, retry, p.engine, p.state, loaded, header,
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
                let header = !loaded.messages.last().is_some_and(|m| m.role == Role::Tool);
                if header {
                    reply_header(ui, &pal, who, None, true);
                }
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
                    let root = p.state.workspace.root.as_deref();
                    for (k, card) in pending_cards(p.state, loaded).iter().enumerate() {
                        tool_card(ui, &pal, (c, "live-tool", k), card, root);
                    }
                    if let Some(st) = p.state.turn_status.get(&c) {
                        ui.label(theme::itext_styled(
                            icon::ARROW_CLOCKWISE,
                            st,
                            theme::small_size(),
                            pal.warn,
                        ));
                    }
                    typing_dots(ui, &pal);
                });
                ui.add_space(8.0);
            }
            for req in p.state.approvals.iter().filter(|a| a.conversation == c) {
                approval_card(ui, &pal, req, &mut |cmd| p.engine.send(cmd));
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

    use crate::state::UiState;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;
    use void_core::engine::Event;
    use void_core::model::{Conversation, Part, Workspace};
    use void_core::permissions::{ApprovalKind, ApprovalRequest, Decision};

    const ROOT: &str = "/home/u/proj";

    fn harness<S: 'static>(
        state: S,
        mut draw: impl FnMut(&mut Ui, &mut S) + 'static,
    ) -> Harness<'static, S> {
        Harness::builder()
            .with_size([760.0, 900.0])
            .with_context_setup(|ctx| {
                theme::install_fonts(ctx);
                theme::apply(ctx, void_core::config::Theme::Dark, 14.0);
            })
            .build_ui_state(move |ui, s| draw(ui, s), state)
    }

    fn call(id: &str, name: &str, args: serde_json::Value) -> Part {
        Part::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args.to_string(),
        }
    }

    fn result(id: &str, content: &str) -> Message {
        Message::new(
            Role::Tool,
            vec![Part::ToolResult {
                call_id: id.into(),
                content: content.into(),
            }],
        )
    }

    /// A Claude Code chat (in a folder workspace) with one assistant message.
    fn claude_chat(parts: Vec<Part>, results: Vec<Message>) -> (UiState, Conversation) {
        let mut state = UiState::default();
        state.workspace = Workspace {
            id: void_core::model::WorkspaceId("ws".into()),
            name: "proj".into(),
            root: Some(ROOT.into()),
        };
        let mut c = Conversation::new(state.workspace.id.clone());
        c.messages.push(Message::user("go"));
        c.messages.push(Message::new(Role::Assistant, parts));
        c.messages.extend(results);
        (state, c)
    }

    fn cards_harness(state: UiState, c: Conversation) -> Harness<'static, (UiState, Conversation)> {
        harness((state, c), |ui, (state, c)| {
            let pal = theme::palette_of(ui);
            let m = c.messages[1].clone();
            tool_cards(ui, &pal, state, c, &m, 1);
        })
    }

    /// Claude Code's own tools get their own cards: Bash like void's bash (command, output),
    /// Edit with its path (workspace-relative) and a short diff, anything else generic (name,
    /// one-line arguments expandable to pretty JSON, result) with MCP tools as "server · tool".
    #[test]
    fn claude_tool_cards_render() {
        let (state, c) = claude_chat(
            vec![
                Part::Text("Running the tests.".into()),
                call(
                    "b1",
                    "Bash",
                    serde_json::json!({"command": "cargo test -p void-core", "description": "Run tests"}),
                ),
                call(
                    "e1",
                    "Edit",
                    serde_json::json!({
                        "file_path": format!("{ROOT}/src/lib.rs"),
                        "old_string": "fn old() {}",
                        "new_string": "fn new() {}\nfn more() {}",
                    }),
                ),
                call(
                    "m1",
                    "mcp__github__create_issue",
                    serde_json::json!({"title": "Flaky SSE test", "labels": ["bug"]}),
                ),
            ],
            vec![
                result("b1", "test result: ok. 208 passed"),
                result("e1", "The file has been updated."),
                result("m1", "Created issue #12"),
            ],
        );
        let mut h = cards_harness(state, c);
        h.run();
        // Bash: the command on the card, the output behind "output".
        assert!(h.query_by_label("Bash").is_some());
        assert!(h.query_by_label("cargo test -p void-core").is_some());
        assert!(h.query_by_label_contains("208 passed").is_none());
        // Edit: relative path and the change itself.
        assert!(h.query_by_label("Edit").is_some());
        assert!(h.query_by_label("src/lib.rs").is_some());
        assert!(
            h.query_by_label_contains(ROOT).is_none(),
            "absolute path shown"
        );
        assert!(h.query_by_label("- fn old() {}").is_some());
        assert!(h.query_by_label("+ fn new() {}").is_some());
        assert!(h.query_by_label("+ fn more() {}").is_some());
        // MCP tool: generic card.
        assert!(h.query_by_label("github · create_issue").is_some());
        assert!(
            h.query_by_label_contains(r#""title":"Flaky SSE test""#)
                .is_some()
        );
        assert!(h.query_by_label_contains("\"labels\": [").is_none());
        h.get_by_label("arguments").click();
        h.run();
        assert!(
            h.query_by_label_contains("\"labels\": [").is_some(),
            "pretty JSON"
        );
        // Results open from their cards.
        h.get_all_by_label("output").next().unwrap().click();
        h.run();
        assert!(h.query_by_label_contains("208 passed").is_some());
    }

    /// A subagent's work streams onto its parent Task card, collapsed: no top-level cards of
    /// its own, its steps behind one header.
    #[test]
    fn subagent_parts_nest_under_parent() {
        let (mut state, c) = claude_chat(
            vec![call(
                "t1",
                "Task",
                serde_json::json!({"description": "Find the SSE parser", "prompt": "Look for it", "subagent_type": "general-purpose"}),
            )],
            vec![result("t1", "Found it: src/backend/sse.rs")],
        );
        let id = c.id;
        state.apply(Event::ToolStarted {
            conversation: id,
            call_id: "t1".into(),
            name: "Task".into(),
            summary: "Find the SSE parser".into(),
        });
        for text in [
            "› Grep after_cr\n",
            "› Read /home/u/proj/src/sse.rs\n",
            "The parser is in sse.rs.",
        ] {
            state.apply(Event::ToolOutput {
                conversation: id,
                call_id: "t1".into(),
                text: text.into(),
            });
        }
        state.apply(Event::ToolFinished {
            conversation: id,
            call_id: "t1".into(),
            output: void_core::tools::ToolOutput::ok("Found it: src/backend/sse.rs"),
        });
        let mut h = cards_harness(state, c);
        h.run();
        assert!(h.query_by_label("Task").is_some());
        assert!(h.query_by_label("Find the SSE parser").is_some());
        // No cards of the subagent's own, and its steps start collapsed.
        assert!(h.query_by_label("Grep").is_none());
        assert!(h.query_by_label("Read").is_none());
        assert!(h.query_by_label_contains("Grep after_cr").is_none());
        let header = h.get_by_label_contains("subagent · 2 steps");
        header.click();
        h.run();
        assert!(h.query_by_label_contains("Grep after_cr").is_some());
        assert!(
            h.query_by_label_contains("The parser is in sse.rs.")
                .is_some()
        );
    }

    fn sent_decisions(cmds: &[Command]) -> Vec<(u64, Decision)> {
        cmds.iter()
            .filter_map(|c| match c {
                Command::Approve { id, decision } => Some((*id, decision.clone())),
                _ => None,
            })
            .collect()
    }

    fn approval_harness(req: ApprovalRequest) -> Harness<'static, Vec<Command>> {
        harness(Vec::new(), move |ui, sent: &mut Vec<Command>| {
            let pal = theme::palette_of(ui);
            approval_card(ui, &pal, &req, &mut |c| sent.push(c));
        })
    }

    fn claude_req(id: u64, always_allow: bool) -> ApprovalRequest {
        ApprovalRequest {
            id,
            conversation: ConversationId::new(),
            tool: "Bash".into(),
            summary: "cargo test".into(),
            command: Some("cargo test -p void-core".into()),
            kind: ApprovalKind::ClaudeCode {
                always_allow,
                title: always_allow.then(|| "Claude wants to run the tests".into()),
                description: always_allow.then(|| "Runs cargo test".into()),
                blocked_path: always_allow.then(|| "/home/u/other".into()),
            },
        }
    }

    /// Claude Code's dialog names Claude Code, shows its own wording, and offers "Always allow"
    /// only when Claude Code suggested a rule; that button sends `AlwaysAsSuggested` (never
    /// void's prefix rule), the others Once and Deny.
    #[test]
    fn always_allow_only_when_offered() {
        let mut h = approval_harness(claude_req(43, false));
        h.run();
        assert!(h.query_by_label("Claude Code wants to use Bash").is_some());
        assert!(h.query_by_label_contains("void wants").is_none());
        assert!(h.query_by_label_contains("Always allow").is_none());
        h.get_by_label_contains("Deny").click();
        h.run();
        assert_eq!(sent_decisions(h.state()), [(43, Decision::Deny)]);

        let mut h = approval_harness(claude_req(44, true));
        h.run();
        assert!(h.query_by_label("Claude wants to run the tests").is_some());
        assert!(h.query_by_label("Runs cargo test").is_some());
        assert!(h.query_by_label_contains("/home/u/other").is_some());
        assert!(h.query_by_label_contains("Always allow `").is_none());
        h.get_by_label_contains("Always allow").click();
        h.run();
        h.get_by_label_contains("Allow once").click();
        h.run();
        assert_eq!(
            sent_decisions(h.state()),
            [(44, Decision::AlwaysAsSuggested), (44, Decision::Once)]
        );

        // void's own requests keep their prefix rule.
        let mut req = claude_req(45, false);
        req.tool = "bash".into();
        req.kind = ApprovalKind::Void;
        let mut h = approval_harness(req);
        h.run();
        assert!(h.query_by_label("void wants to use bash").is_some());
        h.get_by_label_contains("Always allow `cargo test`").click();
        h.run();
        assert_eq!(
            sent_decisions(h.state()),
            [(45, Decision::AlwaysForWorkspace("cargo test".into()))]
        );
    }
}
