//! The dock's panels. Panels read `UiState`, send engine commands, and ask the app for dock
//! changes through `Action`s (the dock is borrowed while panels draw).

// Placeholders until the panels land (Task 8); remove then.
#![allow(dead_code)]

use std::collections::HashMap;

use egui::{Id, Ui, WidgetText};
use void_core::engine::EngineHandle;
use void_core::model::ConversationId;

use crate::layout::Tab;
use crate::state::UiState;

/// Dock changes a panel asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Show this conversation in the last focused Chat tab (or a new one).
    Show(ConversationId),
    /// Show it in a new Chat tab.
    ShowInNewTab(ConversationId),
}

pub struct Panels<'a> {
    pub state: &'a mut UiState,
    pub engine: &'a EngineHandle,
    pub inputs: &'a mut HashMap<u32, String>,
    pub actions: &'a mut Vec<Action>,
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
            Tab::Chat { .. } => "Chat".into(),
            Tab::Conversations => "Conversations".into(),
            Tab::Backends => "Backends".into(),
            Tab::Settings => "Settings".into(),
        }
    }

    fn ui(&mut self, ui: &mut Ui, tab: &mut Tab) {
        ui.label(format!("{} — coming next", tab.kind()));
    }
}
