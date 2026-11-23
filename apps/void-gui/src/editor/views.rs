//! Disk and chat views on an editor tab: the header with the view switch, the "void has
//! pending changes" banner on Disk tabs, and what the banners' buttons do.

use egui::{CornerRadius, Rect, Ui, UiBuilder, vec2};
use void_core::model::ConversationId;

use super::DocKey;
use super::draw::{Banner, BannerAction};
use super::state::Msg;
use crate::layout::EditorView;
use crate::panels::{Action, Panels};
use crate::theme::{self, icon, with_alpha};

/// Height of the strip above the banners (folder workspaces only: Scratch has no chat views).
pub const HEADER_H: f32 = 28.0;

/// On a Disk tab: the chats with pending changes to its file. One chat gets its name and
/// buttons; several collapse into a count and a menu.
pub fn pending_banner(p: &Panels, key: &DocKey) -> Option<Banner> {
    if key.1 != EditorView::Disk {
        return None;
    }
    let rel = p.editors.overlays.as_ref()?.relative(&key.0)?;
    let chats = p.state.pending_for(&rel);
    let (text, buttons, menu) = match chats.as_slice() {
        [] => return None,
        [c] => (
            format!("void has pending changes in {}", p.state.chat_title(*c)),
            vec![
                ("Review", BannerAction::Review(*c)),
                ("View void's version", BannerAction::ViewChat(*c)),
            ],
            Vec::new(),
        ),
        many => (
            format!("{} chats have pending changes", many.len()),
            Vec::new(),
            many.iter().map(|c| (p.state.chat_title(*c), *c)).collect(),
        ),
    };
    Some(Banner {
        icon: icon::GIT_DIFF,
        text,
        color: |p| p.accent,
        buttons,
        menu,
    })
}

/// The tab's header: the file's workspace path and the view switch (Disk, or a chat's
/// version). A chat view is tinted in the accent colour.
pub fn header(ui: &mut Ui, p: &mut Panels, tab: u32, key: &DocKey, r: Rect) {
    let pal = theme::palette_of(ui);
    let current = match key.1 {
        EditorView::Chat(c) => Some(c),
        EditorView::Disk => None,
    };
    if current.is_some() {
        ui.painter()
            .rect_filled(r, CornerRadius::same(4), with_alpha(pal.accent, 30));
    }
    let inner = r.shrink2(vec2(6.0, 1.0));
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(inner)
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    let mut chats = p.view_chats.clone();
    if let Some(c) = current
        && !chats.contains(&c)
    {
        chats.push(c);
    }
    let label = |c: Option<ConversationId>| match c {
        None => theme::itext_small(icon::FLOPPY, "Disk"),
        Some(c) => theme::itext_styled(
            icon::ROBOT,
            &format!("void's version · {}", p.state.chat_title(c)),
            theme::small_size(),
            pal.accent,
        ),
    };
    let mut pick = current;
    let combo = egui::ComboBox::from_id_salt(("editor-view", tab))
        .selected_text(label(current))
        .show_ui(&mut child, |ui| {
            ui.selectable_value(&mut pick, None, label(None));
            for c in &chats {
                ui.selectable_value(&mut pick, Some(*c), label(Some(*c)));
            }
        })
        .response
        .on_hover_text(
            "Which version this tab shows: the file on disk, or a chat's pending changes",
        );
    // The file's path, left of the switch. Painted, not a label: the tab title and the file
    // tree already name the file for assistive tech.
    let shown = p
        .editors
        .overlays
        .as_ref()
        .and_then(|o| o.relative(&key.0))
        .unwrap_or_else(|| key.0.clone());
    let room = Rect::from_min_max(
        inner.min,
        egui::pos2(combo.rect.left() - 8.0, inner.bottom()),
    );
    ui.painter_at(room).text(
        egui::pos2(room.left() + 2.0, room.center().y),
        egui::Align2::LEFT_CENTER,
        shown.display().to_string(),
        egui::FontId::proportional(theme::small_size()),
        pal.text_faint,
    );
    if pick != current {
        let view = pick.map_or(EditorView::Disk, EditorView::Chat);
        p.actions.push(Action::SwitchEditorView { tab, view });
    }
}

/// A banner button on `key`'s tab.
pub fn banner_action(p: &mut Panels, tab: u32, key: &DocKey, action: BannerAction) {
    let chat = key.1 != EditorView::Disk;
    let path = &key.0;
    match action {
        BannerAction::Reload => {
            let r = p.editors.reload(key);
            if let Some(doc) = p.editors.doc_mut(key) {
                doc.core.message = Some(match r {
                    Ok(()) => Msg::info(format!("\"{}\" reloaded", super::file_name(path))),
                    Err(e) => Msg::error(e),
                });
            }
        }
        BannerAction::KeepMine if chat => p.editors.keep_mine_chat(key),
        BannerAction::KeepMine => {
            if let Some(doc) = p.editors.doc_mut(key) {
                doc.keep_mine(path);
            }
        }
        BannerAction::Overwrite if chat => {
            let _ = p.editors.save_chat(key, true);
        }
        BannerAction::Overwrite => {
            if let Some(doc) = p.editors.doc_mut(key) {
                doc.core.message = Some(match doc.core.save_forced(path) {
                    Ok(m) => Msg::info(m),
                    Err(e) => Msg::error(e),
                });
            }
        }
        BannerAction::Review(conversation) => {
            if let Some(rel) = p.editors.overlays.as_ref().and_then(|o| o.relative(path)) {
                p.actions.push(Action::ReviewFile {
                    conversation,
                    path: rel,
                });
            }
        }
        BannerAction::ViewChat(c) => p.actions.push(Action::SwitchEditorView {
            tab,
            view: EditorView::Chat(c),
        }),
    }
}
