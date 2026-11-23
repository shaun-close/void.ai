//! The dock's tabs, the default arrangement, and helpers to reopen or persist it.

use std::path::PathBuf;

use egui_dock::{DockState, NodeIndex};
use serde::{Deserialize, Serialize};
use void_core::model::ConversationId;

/// What an editor tab shows of its file: the real file, or a chat's pending version of it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EditorView {
    Disk,
    Chat(ConversationId),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tab {
    /// `id` tells Chat tabs apart; `conversation` is what it shows.
    Chat {
        id: u32,
        conversation: Option<ConversationId>,
    },
    Conversations,
    Backends,
    Settings,
    Review,
    /// `id` tells tabs on the same file and view apart (they share one buffer).
    Editor {
        id: u32,
        path: PathBuf,
        view: EditorView,
    },
}

impl Tab {
    pub fn kind(&self) -> &'static str {
        match self {
            Tab::Chat { .. } => "chat",
            Tab::Conversations => "conversations",
            Tab::Backends => "backends",
            Tab::Settings => "settings",
            Tab::Review => "review",
            Tab::Editor { .. } => "editor",
        }
    }

    /// The same dock slot: the same panel, or the same Chat tab (whatever it shows).
    fn same_slot(&self, other: &Tab) -> bool {
        match (self, other) {
            (Tab::Chat { id: a, .. }, Tab::Chat { id: b, .. }) => a == b,
            (Tab::Editor { id: a, .. }, Tab::Editor { id: b, .. }) => a == b,
            _ => self.kind() == other.kind(),
        }
    }
}

/// Conversations on the left, Chat in the middle, Backends and Settings tabbed on the right.
pub fn default_layout() -> DockState<Tab> {
    let mut d = DockState::new(vec![Tab::Chat {
        id: 1,
        conversation: None,
    }]);
    let tree = d.main_surface_mut();
    let [chat, _] = tree.split_right(
        NodeIndex::root(),
        0.72,
        vec![Tab::Backends, Tab::Settings, Tab::Review],
    );
    tree.split_left(chat, 0.3, vec![Tab::Conversations]);
    d
}

/// Focus the tab if it's open anywhere, otherwise add it to the focused leaf.
pub fn ensure_tab(d: &mut DockState<Tab>, tab: Tab) {
    if let Some(path) = d.find_tab_from(|t| t.same_slot(&tab)) {
        let _ = d.set_active_tab(path);
        d.set_focused_node_and_surface(path.node_path());
    } else {
        d.push_to_focused_leaf(tab);
    }
}

pub fn next_editor_id(d: &DockState<Tab>) -> u32 {
    d.iter_all_tabs()
        .filter_map(|(_, t)| match t {
            Tab::Editor { id, .. } => Some(*id),
            _ => None,
        })
        .max()
        .unwrap_or(0)
        + 1
}

pub fn next_chat_id(d: &DockState<Tab>) -> u32 {
    d.iter_all_tabs()
        .filter_map(|(_, t)| match t {
            Tab::Chat { id, .. } => Some(*id),
            _ => None,
        })
        .max()
        .unwrap_or(0)
        + 1
}

/// Layouts are stored as RON: unlike JSON it can hold the infinite rects egui_dock uses before
/// a layout is first drawn.
pub fn to_text(d: &DockState<Tab>) -> String {
    ron::to_string(d).unwrap_or_default()
}

pub fn from_text(text: &str) -> Option<DockState<Tab>> {
    ron::from_str(text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(d: &DockState<Tab>) -> Vec<&'static str> {
        let mut v: Vec<_> = d.iter_all_tabs().map(|(_, t)| t.kind()).collect();
        v.sort();
        v
    }

    #[test]
    fn default_layout_has_all_tabs() {
        assert_eq!(
            kinds(&default_layout()),
            ["backends", "chat", "conversations", "review", "settings"]
        );
    }

    #[test]
    fn ensure_tab_reopens_closed_tab() {
        let mut d = default_layout();
        let path = d.find_tab(&Tab::Settings).unwrap();
        d.remove_tab(path);
        assert!(d.find_tab(&Tab::Settings).is_none());
        ensure_tab(&mut d, Tab::Settings);
        assert_eq!(
            d.iter_all_tabs()
                .filter(|(_, t)| **t == Tab::Settings)
                .count(),
            1
        );
    }

    #[test]
    fn ensure_tab_does_not_duplicate() {
        let mut d = default_layout();
        ensure_tab(&mut d, Tab::Conversations);
        ensure_tab(&mut d, Tab::Conversations);
        assert_eq!(
            d.iter_all_tabs()
                .filter(|(_, t)| **t == Tab::Conversations)
                .count(),
            1
        );
    }

    #[test]
    fn new_chat_tab_gets_a_fresh_id() {
        let mut d = default_layout();
        let id = next_chat_id(&d);
        ensure_tab(
            &mut d,
            Tab::Chat {
                id,
                conversation: None,
            },
        );
        let ids: Vec<u32> = d
            .iter_all_tabs()
            .filter_map(|(_, t)| match t {
                Tab::Chat { id, .. } => Some(*id),
                _ => None,
            })
            .collect();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]);
    }

    #[test]
    fn layout_text_roundtrip() {
        let d = default_layout();
        let json = to_text(&d);
        let back = from_text(&json).unwrap();
        assert_eq!(to_text(&back), json);
        assert!(from_text("{not json").is_none());
    }

    #[test]
    fn editor_tabs_roundtrip() {
        let mut d = default_layout();
        ensure_tab(
            &mut d,
            Tab::Editor {
                id: 1,
                path: "/tmp/a b/é.rs".into(),
                view: EditorView::Disk,
            },
        );
        ensure_tab(
            &mut d,
            Tab::Editor {
                id: 2,
                path: "/tmp/x.rs".into(),
                view: EditorView::Chat(ConversationId::new()),
            },
        );
        let text = to_text(&d);
        let back = from_text(&text).unwrap();
        assert_eq!(to_text(&back), text);
        assert_eq!(next_editor_id(&back), 3);
    }
}
