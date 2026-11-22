//! The dock's panels. Panels read `UiState`, send engine commands, and ask the app for dock
//! changes through `Action`s (the dock is borrowed while panels draw).

use std::collections::HashMap;

use egui::{Id, Ui, WidgetText};
use void_core::engine::EngineHandle;
use void_core::model::ConversationId;

pub mod backends;
pub mod chat;
pub mod conversations;
pub mod settings;

use void_core::engine::Command;

use crate::layout::Tab;
use crate::state::UiState;

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
}

pub struct Panels<'a> {
    pub state: &'a mut UiState,
    pub engine: &'a EngineHandle,
    pub inputs: &'a mut HashMap<u32, String>,
    pub actions: &'a mut Vec<Action>,
    pub list: &'a mut conversations::ListUi,
    pub backends: &'a mut backends::BackendsUi,
    /// The conversation in the focused Chat tab (highlighted in the list).
    pub focused: Option<ConversationId>,
}

impl egui_dock::TabViewer for Panels<'_> {
    type Tab = Tab;

    fn id(&mut self, tab: &mut Tab) -> Id {
        match tab {
            Tab::Chat { id, .. } => Id::unique(("chat", *id)),
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
                chat::tab_title(title.as_deref()).into()
            }
            Tab::Conversations => "Conversations".into(),
            Tab::Backends => "Backends".into(),
            Tab::Settings => "Settings".into(),
        }
    }

    fn ui(&mut self, ui: &mut Ui, tab: &mut Tab) {
        match tab {
            Tab::Chat { id, conversation } => chat::show(ui, self, *id, *conversation),
            Tab::Conversations => conversations::show(ui, self),
            Tab::Backends => backends::show(ui, self),
            Tab::Settings => settings::show(ui, self),
        }
    }
}
