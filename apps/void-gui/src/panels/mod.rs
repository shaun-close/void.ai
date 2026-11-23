//! The dock's panels. Panels read `UiState`, send engine commands, and ask the app for dock
//! changes through `Action`s (the dock is borrowed while panels draw).

use std::collections::HashMap;

use egui::{Id, Ui, WidgetText};
use void_core::engine::EngineHandle;
use void_core::model::ConversationId;

pub mod backends;
pub mod chat;
pub mod conversations;
pub mod files;
pub mod review;
pub mod settings;

use void_core::engine::Command;

use crate::layout::{EditorView, Tab};
use crate::state::UiState;
use crate::theme::{self, icon};

/// Persist the whole draft config (Settings' Save) and remember it as the saved version.
pub fn save_config(state: &mut UiState, engine: &EngineHandle) {
    state.saved_config = state.config.clone();
    engine.send(Command::SaveConfig(state.config.clone()));
}

/// Persist one change on top of the saved config, leaving other unsaved drafts alone.
pub fn save_change(
    state: &mut UiState,
    engine: &EngineHandle,
    change: impl Fn(&mut void_core::config::Config),
) {
    engine.send(Command::SaveConfig(state.save_change(change)));
}

/// Dock changes a panel asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Show this conversation in the last focused Chat tab (or a new one).
    Show(ConversationId),
    /// Show it in a new Chat tab.
    ShowInNewTab(ConversationId),
    /// A Chat tab with no conversation sent its first message: create one, then send.
    StartChat { tab: u32, text: String },
    /// Open a workspace file (relative to the workspace root) in an editor tab.
    OpenFile(std::path::PathBuf),
    /// Another tab on the same buffer as this editor tab.
    DuplicateEditor(u32),
    /// The user closed this editor tab.
    CloseEditor(u32),
    /// The Files panel renamed a file or folder: open editors follow it.
    Renamed {
        from: std::path::PathBuf,
        to: std::path::PathBuf,
    },
    /// Bring Review forward on this chat's change to `path` (workspace-relative).
    ReviewFile {
        conversation: ConversationId,
        path: std::path::PathBuf,
    },
    /// Show another version of an editor tab's file (Disk, or a chat's).
    SwitchEditorView { tab: u32, view: EditorView },
}

pub struct Panels<'a> {
    pub state: &'a mut UiState,
    pub engine: &'a EngineHandle,
    pub inputs: &'a mut HashMap<u32, String>,
    pub actions: &'a mut Vec<Action>,
    pub list: &'a mut conversations::ListUi,
    pub backends: &'a mut backends::BackendsUi,
    pub md: &'a mut egui_commonmark::CommonMarkCache,
    pub review: &'a mut review::ReviewUi,
    /// The conversation in the focused Chat tab (highlighted in the list).
    pub focused: Option<ConversationId>,
    pub editors: &'a mut crate::editor::EditorStore,
    /// The editor tab the dock has focused, if it's one.
    pub focused_editor: Option<u32>,
    pub files: &'a mut files::FilesUi,
    /// The file in the editor tab focused last (the Files panel highlights and reveals it).
    pub active_file: Option<std::path::PathBuf>,
    /// Chats an editor tab can switch to: those with a layer of pending changes, or open in a
    /// Chat tab.
    pub view_chats: Vec<ConversationId>,
}

impl egui_dock::TabViewer for Panels<'_> {
    type Tab = Tab;

    fn id(&mut self, tab: &mut Tab) -> Id {
        match tab {
            Tab::Chat { id, .. } => Id::unique(("chat", *id)),
            Tab::Editor { id, .. } => Id::unique(("editor", *id)),
            other => Id::unique(other.kind()),
        }
    }

    fn title(&mut self, tab: &mut Tab) -> WidgetText {
        match tab {
            Tab::Chat { conversation, .. } => {
                let title = conversation.and_then(|c| {
                    self.state
                        .loaded
                        .get(&c)
                        .map(|c| c.title.clone())
                        .or_else(|| {
                            self.state
                                .conversations
                                .iter()
                                .find(|m| m.id == c)
                                .map(|m| m.title.clone())
                        })
                });
                theme::itext(icon::CHAT, &chat::tab_title(title.as_deref()))
            }
            Tab::Conversations => theme::itext(icon::CHATS, "Chats"),
            Tab::Backends => theme::itext(icon::PLUGS, "Backends"),
            Tab::Settings => theme::itext(icon::GEAR, "Settings"),
            Tab::Files => theme::itext(icon::FOLDER, "Files"),
            Tab::Review => {
                let n = self
                    .focused
                    .and_then(|c| self.state.changes.get(&c))
                    .map_or(0, Vec::len);
                if n > 0 {
                    theme::itext(icon::GIT_DIFF, &format!("Review · {n}"))
                } else {
                    theme::itext(icon::GIT_DIFF, "Review")
                }
            }
            Tab::Editor { path, view, .. } => {
                let key = (path.clone(), view.clone());
                let mut name = crate::editor::file_name(path);
                if let EditorView::Chat(_) = view {
                    name.push_str(" · chat");
                }
                if self.editors.is_dirty(&key) {
                    name.push_str(" ●");
                }
                theme::itext(crate::editor::lang_icon(path), &name)
            }
        }
    }

    fn on_tab_button(&mut self, tab: &mut Tab, response: &egui::Response) {
        if let Tab::Editor { path, .. } = tab {
            response.clone().on_hover_text(path.display().to_string());
        }
    }

    fn context_menu(&mut self, ui: &mut Ui, tab: &mut Tab, _path: egui_dock::NodePath) {
        if let Tab::Editor { id, path, .. } = tab {
            if ui
                .button(theme::itext(icon::LAYOUT, "Open in another tab"))
                .clicked()
            {
                self.actions.push(Action::DuplicateEditor(*id));
                ui.close();
            }
            if ui.button(theme::itext(icon::COPY, "Copy path")).clicked() {
                ui.ctx().copy_text(path.display().to_string());
                ui.close();
            }
        }
    }

    /// Editor tabs close through the app, which asks Save / Discard / Cancel when the last tab
    /// on a dirty buffer goes.
    fn on_close(&mut self, tab: &mut Tab) -> egui_dock::tab_viewer::OnCloseResponse {
        use egui_dock::tab_viewer::OnCloseResponse;
        if let Tab::Editor { id, .. } = tab {
            self.actions.push(Action::CloseEditor(*id));
            return OnCloseResponse::Ignore;
        }
        OnCloseResponse::Close
    }

    /// Panels scroll themselves; the dock's own scroll areas would stop text from wrapping.
    fn scroll_bars(&self, _tab: &Tab) -> [bool; 2] {
        [false, false]
    }

    fn ui(&mut self, ui: &mut Ui, tab: &mut Tab) {
        // Keep anything too wide for a narrow panel inside that panel.
        let clip = ui.clip_rect().intersect(ui.max_rect());
        ui.set_clip_rect(clip);
        match tab {
            Tab::Chat { id, conversation } => chat::show(ui, self, *id, *conversation),
            Tab::Conversations => conversations::show(ui, self),
            Tab::Backends => backends::show(ui, self),
            Tab::Settings => settings::show(ui, self),
            Tab::Review => review::show(ui, self),
            Tab::Files => files::show(ui, self),
            Tab::Editor { id, path, view } => crate::editor::show(ui, self, *id, path, view),
        }
    }
}
