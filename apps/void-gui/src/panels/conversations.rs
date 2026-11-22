//! The Conversations panel: the workspace's chats, newest first.

use chrono::{DateTime, Datelike, Utc};
use egui::{CornerRadius, Key, Response, RichText, Sense, TextEdit, Ui, Vec2};
use void_core::engine::Command;
use void_core::model::ConversationId;

use super::{Action, Panels};
use crate::theme::{self, BOLD, Palette, icon, lerp};

/// Transient list state: an inline rename or a delete waiting for confirmation.
#[derive(Debug, Default)]
pub struct ListUi {
    pub filter: String,
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
    let pal = theme::palette_of(ui);
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("Chats")
                .family(egui::FontFamily::Name(BOLD.into()))
                .size(16.0)
                .color(pal.text),
        );
        ui.label(
            RichText::new(p.state.conversations.len().to_string())
                .small()
                .color(pal.text_faint),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if theme::primary_button(ui, icon::PLUS, "New").clicked() {
                p.engine.send(Command::NewConversation);
            }
        });
    });
    ui.add_space(2.0);
    ui.add(
        TextEdit::singleline(&mut p.list.filter)
            .hint_text(theme::itext_styled(
                icon::MAGNIFIER,
                "Search chats",
                theme::body_size(),
                pal.text_faint,
            ))
            .desired_width(f32::INFINITY),
    );
    ui.add_space(4.0);
    if p.state.conversations.is_empty() {
        ui.add_space(20.0);
        ui.vertical_centered(|ui| {
            ui.label(theme::ico(icon::CHATS).size(28.0).color(pal.text_faint));
            ui.label(RichText::new("No chats yet").color(pal.text_faint));
        });
        return;
    }
    let now = Utc::now();
    let needle = p.list.filter.trim().to_lowercase();
    let metas: Vec<_> = p
        .state
        .conversations
        .iter()
        .filter(|m| needle.is_empty() || m.title.to_lowercase().contains(&needle))
        .cloned()
        .collect();
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
                    theme::card(ui, |ui| {
                        ui.label(format!("Delete \u{201c}{}\u{201d}?", m.title));
                        ui.horizontal(|ui| {
                            let del = egui::Button::new(theme::itext_styled(
                                icon::TRASH,
                                "Delete",
                                theme::body_size(),
                                egui::Color32::WHITE,
                            ))
                            .fill(pal.err);
                            if ui.add(del).clicked() {
                                p.engine.send(Command::DeleteConversation(m.id));
                                p.list.confirm_delete = None;
                            }
                            if ui.button("Cancel").clicked() {
                                p.list.confirm_delete = None;
                            }
                        });
                    });
                    continue;
                }
                let selected = p.focused == Some(m.id);
                let streaming = p.state.streams.contains_key(&m.id);
                let r = row(
                    ui,
                    &pal,
                    &m.title,
                    &relative(m.updated, now),
                    selected,
                    streaming,
                );
                if r.clicked() {
                    p.actions.push(Action::Show(m.id));
                }
                r.context_menu(|ui| {
                    if ui
                        .button(theme::itext(icon::LAYOUT, "Open in new tab"))
                        .clicked()
                    {
                        p.actions.push(Action::ShowInNewTab(m.id));
                    }
                    if ui.button(theme::itext(icon::PENCIL, "Rename")).clicked() {
                        p.list.renaming = Some((m.id, m.title.clone(), false));
                    }
                    if ui
                        .button(theme::itext_styled(
                            icon::TRASH,
                            "Delete",
                            theme::body_size(),
                            pal.err,
                        ))
                        .clicked()
                    {
                        p.list.confirm_delete = Some(m.id);
                    }
                });
            }
        });
}

/// One conversation: a rounded row with an accent bar when selected.
fn row(
    ui: &mut Ui,
    pal: &Palette,
    title: &str,
    when: &str,
    selected: bool,
    streaming: bool,
) -> Response {
    let height = 46.0;
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click());
    let painter = ui.painter_at(rect);
    let fill = if selected {
        lerp(pal.bg_raised, pal.accent, 0.16)
    } else if resp.hovered() {
        pal.bg_hover
    } else {
        egui::Color32::TRANSPARENT
    };
    painter.rect_filled(rect, CornerRadius::same(8), fill);
    if selected {
        let bar = egui::Rect::from_min_size(
            rect.min + Vec2::new(0.0, 8.0),
            Vec2::new(3.0, height - 16.0),
        );
        painter.rect_filled(bar, CornerRadius::same(2), pal.accent);
    }
    let left = rect.left() + 14.0;
    let title_color = if selected { pal.text } else { pal.text_dim };
    let max_w = rect.width() - 28.0;
    let galley = ui.fonts_mut(|f| {
        f.layout(
            title.to_string(),
            egui::FontId::proportional(ui.style().text_styles[&egui::TextStyle::Body].size),
            title_color,
            max_w,
        )
    });
    let first_row = galley.rows.first().map(|r| r.glyphs.len()).unwrap_or(0);
    let shown: String = if first_row < title.chars().count() {
        let mut s: String = title.chars().take(first_row.saturating_sub(1)).collect();
        s.push('…');
        s
    } else {
        title.to_string()
    };
    painter.text(
        egui::pos2(left, rect.top() + 7.0),
        egui::Align2::LEFT_TOP,
        shown,
        egui::FontId::proportional(ui.style().text_styles[&egui::TextStyle::Body].size),
        title_color,
    );
    let mut meta = when.to_string();
    if streaming {
        meta = format!("● replying · {meta}");
    }
    painter.text(
        egui::pos2(left, rect.bottom() - 7.0),
        egui::Align2::LEFT_BOTTOM,
        meta,
        egui::FontId::proportional(ui.style().text_styles[&egui::TextStyle::Small].size),
        if streaming {
            pal.accent2
        } else {
            pal.text_faint
        },
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
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
