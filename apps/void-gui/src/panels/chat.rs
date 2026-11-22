//! The Chat panel: one conversation's messages, the streaming reply, and the input box.

use egui::{Id, Key, Label, Modifiers, RichText, TextEdit, Ui};
use void_core::engine::Command;
use void_core::model::{ConversationId, Message, Role};

use super::{Action, Panels};

/// A piece of message text: prose, or a fenced code block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    Text(String),
    Code { lang: String, body: String },
}

/// Split Markdown-ish text on ``` fences. An unterminated fence (mid-stream) runs to the end.
pub fn split_fenced_code(text: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut code: Option<String> = None;
    for line in text.split_inclusive('\n') {
        let fence = line.trim_start().strip_prefix("```");
        match (fence, code.take()) {
            (Some(rest), None) => {
                if !buf.is_empty() {
                    out.push(Segment::Text(std::mem::take(&mut buf)));
                }
                code = Some(rest.trim().to_string());
            }
            (Some(_), Some(lang)) => out.push(Segment::Code {
                lang,
                body: std::mem::take(&mut buf),
            }),
            (None, lang) => {
                code = lang;
                buf.push_str(line);
            }
        }
    }
    match code {
        Some(lang) => out.push(Segment::Code { lang, body: buf }),
        None if !buf.is_empty() => out.push(Segment::Text(buf)),
        None => {}
    }
    out
}

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

fn render_text(ui: &mut Ui, text: &str) {
    for seg in split_fenced_code(text) {
        match seg {
            Segment::Text(t) => {
                let t = t.trim_end_matches('\n');
                if !t.is_empty() {
                    ui.add(Label::new(t).selectable(true).wrap());
                }
            }
            Segment::Code { lang, body } => {
                egui::Frame::new()
                    .fill(ui.visuals().extreme_bg_color)
                    .corner_radius(4)
                    .inner_margin(6)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(if lang.is_empty() { "code" } else { &lang })
                                    .small()
                                    .weak(),
                            );
                            if ui.small_button("Copy").clicked() {
                                ui.ctx().copy_text(body.clone());
                            }
                        });
                        ui.add(
                            Label::new(RichText::new(body.trim_end_matches('\n')).monospace())
                                .selectable(true)
                                .wrap(),
                        );
                    });
            }
        }
    }
}

fn render_reasoning(
    ui: &mut Ui,
    salt: impl std::hash::Hash + std::fmt::Debug,
    text: &str,
    live: bool,
) {
    if text.is_empty() {
        return;
    }
    egui::CollapsingHeader::new(RichText::new(if live { "Thinking…" } else { "Thought" }).weak())
        .id_salt(salt)
        .default_open(false)
        .show(ui, |ui| {
            ui.add(
                Label::new(RichText::new(text).weak().italics())
                    .selectable(true)
                    .wrap(),
            );
        });
}

fn render_message(
    ui: &mut Ui,
    conv: ConversationId,
    index: usize,
    m: &Message,
    is_last: bool,
    engine: &void_core::engine::EngineHandle,
) {
    let you = m.role == Role::User;
    let fill = if you {
        ui.visuals().faint_bg_color
    } else {
        egui::Color32::TRANSPARENT
    };
    egui::Frame::new()
        .fill(fill)
        .corner_radius(6)
        .inner_margin(8)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(if you { "You" } else { "Assistant" }).strong());
            render_reasoning(ui, (conv, index), &m.reasoning(), false);
            render_text(ui, &m.text());
            if m.stopped {
                ui.label(RichText::new("stopped").small().weak());
            }
            if let Some(err) = &m.error {
                ui.colored_label(ui.visuals().error_fg_color, err);
            }
            if is_last && !you && (m.error.is_some() || m.stopped) && ui.button("Retry").clicked() {
                engine.send(Command::Retry(conv));
            }
        });
}

/// Draw a Chat tab. `tab` is its id; `conv` the conversation it shows (None until the first send).
pub fn show(ui: &mut Ui, p: &mut Panels, tab: u32, conv: Option<ConversationId>) {
    let streaming = conv.is_some_and(|c| p.state.streams.contains_key(&c));

    egui::Panel::bottom(("chat-input", tab)).show(ui, |ui| {
        ui.add_space(4.0);
        if let Some(c) = conv
            && let Some(loaded) = p.state.loaded.get(&c)
        {
            let current = loaded.profile_id.clone();
            let mut chosen = current.clone();
            let label = |id: &Option<String>| match id {
                None => format!(
                    "Active profile ({})",
                    p.state
                        .saved_config
                        .profile(&p.state.saved_config.active_profile)
                        .map(|p| p.name.as_str())
                        .unwrap_or("?")
                ),
                Some(id) => p
                    .state
                    .config
                    .profile(id)
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| format!("{id} (missing)")),
            };
            egui::ComboBox::from_id_salt(("chat-profile", tab))
                .selected_text(label(&current))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut chosen, None, label(&None));
                    for prof in &p.state.saved_config.profiles {
                        ui.selectable_value(&mut chosen, Some(prof.id.clone()), &prof.name);
                    }
                });
            if chosen != current {
                p.engine.send(Command::SetConversationProfile(c, chosen));
            }
        }
        let input_id = Id::unique(("chat-input-edit", tab));
        let buf = p.inputs.entry(tab).or_default();
        let mut send = ui.memory(|m| m.has_focus(input_id))
            && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
        ui.add(
            TextEdit::multiline(buf)
                .id(input_id)
                .desired_rows(3)
                .desired_width(f32::INFINITY)
                .hint_text("Message… (Enter sends, Shift+Enter adds a line)"),
        );
        ui.horizontal(|ui| {
            if streaming {
                if ui.button("Stop").clicked()
                    && let Some(c) = conv
                {
                    p.engine.send(Command::Stop(c));
                }
            } else if ui.button("Send").clicked() {
                send = true;
            }
        });
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
        ui.add_space(4.0);
    });

    let Some(c) = conv else {
        ui.centered_and_justified(|ui| {
            ui.label(RichText::new("Type a message below to start a chat.").weak());
        });
        return;
    };
    let Some(loaded) = p.state.loaded.get(&c) else {
        ui.centered_and_justified(|ui| ui.spinner());
        return;
    };
    egui::ScrollArea::vertical()
        .id_salt(("chat-scroll", tab))
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let n = loaded.messages.len();
            for (i, m) in loaded.messages.iter().enumerate() {
                render_message(ui, c, i, m, i + 1 == n && !streaming, p.engine);
                ui.add_space(4.0);
            }
            if let Some(s) = p.state.streams.get(&c) {
                egui::Frame::new().inner_margin(8).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Assistant").strong());
                        ui.spinner();
                    });
                    render_reasoning(ui, (c, "live"), &s.reasoning, s.text.is_empty());
                    render_text(ui, &s.text);
                });
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_fenced_code_finds_blocks() {
        assert_eq!(
            split_fenced_code("a\n```rs\nfn x(){}\n```\nb"),
            vec![
                Segment::Text("a\n".into()),
                Segment::Code {
                    lang: "rs".into(),
                    body: "fn x(){}\n".into()
                },
                Segment::Text("b".into()),
            ]
        );
    }

    #[test]
    fn unterminated_fence_runs_to_the_end() {
        assert_eq!(
            split_fenced_code("x\n```\nlet a = 1;\nlet b"),
            vec![
                Segment::Text("x\n".into()),
                Segment::Code {
                    lang: String::new(),
                    body: "let a = 1;\nlet b".into()
                },
            ]
        );
    }

    #[test]
    fn plain_text_is_one_segment() {
        assert_eq!(
            split_fenced_code("hello"),
            vec![Segment::Text("hello".into())]
        );
        assert!(split_fenced_code("").is_empty());
    }

    #[test]
    fn tab_title_truncates() {
        assert_eq!(tab_title(None), "Chat");
        assert_eq!(tab_title(Some("short")), "short");
        let t = tab_title(Some(&"y".repeat(40)));
        assert_eq!(t.chars().count(), 30);
        assert!(t.ends_with('…'));
    }
}
