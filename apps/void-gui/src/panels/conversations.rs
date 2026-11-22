//! The Conversations panel: the workspace's chats, newest first.

use chrono::{DateTime, Datelike, Utc};
use egui::{Key, RichText, TextEdit, Ui};
use void_core::engine::Command;
use void_core::model::ConversationId;

use super::{Action, Panels};

/// Transient list state: an inline rename or a delete waiting for confirmation.
#[derive(Debug, Default)]
pub struct ListUi {
    renaming: Option<(ConversationId, String, bool)>,
    confirm_delete: Option<ConversationId>,
}

/// "just now", "5 min ago", "3 h ago", "20 Nov", "18 Oct 2025".
pub fn relative(t: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - t).num_seconds();
    if secs < 60 {
        "just now".into()
    } else if secs < 3600 {
        format!("{} min ago", secs / 60)
    } else if secs < 86_400 {
        format!("{} h ago", secs / 3600)
    } else if t.year() == now.year() {
        t.format("%-d %b").to_string()
    } else {
        t.format("%-d %b %Y").to_string()
    }
}

pub fn show(ui: &mut Ui, p: &mut Panels) {
    ui.horizontal(|ui| {
        if ui.button("New chat").clicked() {
            p.engine.send(Command::NewConversation);
        }
        ui.label(RichText::new(&p.state.workspace.name).weak());
    });
    ui.separator();
    if p.state.conversations.is_empty() {
        ui.label(RichText::new("No conversations yet.").weak());
        return;
    }
    let now = Utc::now();
    let metas = p.state.conversations.clone();
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for m in metas {
                if let Some((id, buf, focused)) = &mut p.list.renaming
                    && *id == m.id
                {
                    let r = ui.add(TextEdit::singleline(buf).desired_width(f32::INFINITY));
                    if !*focused {
                        r.request_focus();
                        *focused = true;
                    }
                    if r.lost_focus() {
                        if ui.input(|i| i.key_pressed(Key::Enter)) {
                            p.engine
                                .send(Command::RenameConversation(m.id, buf.clone()));
                        }
                        p.list.renaming = None;
                    }
                    continue;
                }
                if p.list.confirm_delete == Some(m.id) {
                    ui.label(format!("Delete \u{201c}{}\u{201d}?", m.title));
                    ui.horizontal(|ui| {
                        if ui.button("Delete").clicked() {
                            p.engine.send(Command::DeleteConversation(m.id));
                            p.list.confirm_delete = None;
                        }
                        if ui.button("Cancel").clicked() {
                            p.list.confirm_delete = None;
                        }
                    });
                    continue;
                }
                let selected = p.focused == Some(m.id);
                let resp = ui.horizontal(|ui| {
                    let r = ui.selectable_label(selected, &m.title);
                    if p.state.streams.contains_key(&m.id) {
                        ui.spinner();
                    }
                    ui.label(RichText::new(relative(m.updated, now)).small().weak());
                    r
                });
                let r = resp.inner;
                if r.clicked() {
                    p.actions.push(Action::Show(m.id));
                }
                r.context_menu(|ui| {
                    if ui.button("Open in new tab").clicked() {
                        p.actions.push(Action::ShowInNewTab(m.id));
                    }
                    if ui.button("Rename").clicked() {
                        p.list.renaming = Some((m.id, m.title.clone(), false));
                    }
                    if ui.button("Delete").clicked() {
                        p.list.confirm_delete = Some(m.id);
                    }
                });
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone, Utc};

    #[test]
    fn relative_time_reads_naturally() {
        let now = Utc.with_ymd_and_hms(2026, 11, 22, 12, 0, 0).unwrap();
        assert_eq!(relative(now - Duration::seconds(20), now), "just now");
        assert_eq!(relative(now - Duration::minutes(5), now), "5 min ago");
        assert_eq!(relative(now - Duration::hours(3), now), "3 h ago");
        assert_eq!(relative(now - Duration::days(2), now), "20 Nov");
        assert_eq!(relative(now - Duration::days(400), now), "18 Oct 2025");
    }
}
