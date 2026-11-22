//! The UI's mirror of engine state, updated only from engine events.

use std::collections::HashMap;

use void_core::config::Config;
use void_core::engine::{DeltaKind, Event};
use void_core::model::{Conversation, ConversationId, ConversationMeta, Workspace};
use void_core::store::WorkspaceEntry;

/// A reply arriving right now.
#[derive(Clone, Debug, Default)]
pub struct Streaming {
    pub text: String,
    pub reasoning: String,
}

#[derive(Clone, Debug)]
pub struct Status {
    pub text: String,
    pub error: bool,
}

#[derive(Debug)]
pub struct UiState {
    pub config: Config,
    /// The config as last loaded or saved (Settings compares against it).
    pub saved_config: Config,
    pub workspace: Workspace,
    pub recent: Vec<WorkspaceEntry>,
    pub conversations: Vec<ConversationMeta>,
    pub loaded: HashMap<ConversationId, Conversation>,
    pub streams: HashMap<ConversationId, Streaming>,
    pub last_tps: Option<f32>,
    pub tests: HashMap<String, Result<Vec<String>, String>>,
    pub status: Option<Status>,
    /// Set by WorkspaceOpened: the layout the app should apply (None = default).
    layout: Option<Option<String>>,
    /// A conversation the engine just created, for the app to put in a Chat tab.
    created: Option<ConversationId>,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            config: Config::default(),
            saved_config: Config::default(),
            workspace: Workspace::scratch(),
            recent: Vec::new(),
            conversations: Vec::new(),
            loaded: HashMap::new(),
            streams: HashMap::new(),
            last_tps: None,
            tests: HashMap::new(),
            status: None,
            layout: None,
            created: None,
        }
    }
}

impl UiState {
    pub fn apply(&mut self, ev: Event) {
        match ev {
            Event::ConfigLoaded { config, warning } => {
                self.saved_config = config.clone();
                self.config = config;
                if let Some(w) = warning {
                    self.error(w);
                }
            }
            Event::WorkspaceOpened {
                workspace,
                conversations,
                recent,
                layout,
                warnings,
            } => {
                self.workspace = workspace;
                self.conversations = conversations;
                self.recent = recent;
                let streaming: Vec<_> = self.streams.keys().copied().collect();
                self.loaded.retain(|id, _| streaming.contains(id));
                self.layout = Some(layout);
                for w in warnings {
                    self.error(w);
                }
            }
            Event::ConversationList(list) => self.conversations = list,
            Event::Conversation(c) => {
                if c.messages.is_empty() && !self.loaded.contains_key(&c.id) {
                    self.created = Some(c.id);
                }
                self.loaded.insert(c.id, c);
            }
            Event::StreamStarted(id) => {
                self.streams.insert(id, Streaming::default());
            }
            Event::Delta {
                conversation,
                kind,
                text,
            } => {
                let s = self.streams.entry(conversation).or_default();
                match kind {
                    DeltaKind::Text => s.text.push_str(&text),
                    DeltaKind::Reasoning => s.reasoning.push_str(&text),
                }
            }
            Event::StreamFinished {
                conversation,
                message,
                tokens_per_sec,
            } => {
                self.streams.remove(&conversation);
                if let Some(c) = self.loaded.get_mut(&conversation) {
                    c.messages.push(message);
                }
                if tokens_per_sec.is_some() {
                    self.last_tps = tokens_per_sec;
                }
            }
            Event::StreamFailed {
                conversation,
                error,
            } => {
                self.streams.remove(&conversation);
                self.error(error);
            }
            Event::ProfileTested { profile_id, result } => {
                self.tests.insert(profile_id, result);
            }
            Event::ConfigSaved => self.info("Settings saved."),
            Event::Error(e) => self.error(e),
        }
    }

    pub fn info(&mut self, text: impl Into<String>) {
        self.status = Some(Status {
            text: text.into(),
            error: false,
        });
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.status = Some(Status {
            text: text.into(),
            error: true,
        });
    }

    pub fn take_layout(&mut self) -> Option<Option<String>> {
        self.layout.take()
    }

    pub fn take_created(&mut self) -> Option<ConversationId> {
        self.created.take()
    }

    /// The profile a conversation uses (its own, else the active one).
    pub fn profile_for(&self, id: Option<ConversationId>) -> Option<&void_core::config::Profile> {
        let pid = id
            .and_then(|id| self.loaded.get(&id))
            .and_then(|c| c.profile_id.clone())
            .unwrap_or_else(|| self.config.active_profile.clone());
        self.config.profile(&pid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use void_core::engine::{DeltaKind, Event};
    use void_core::model::{Conversation, Message, Part, Role, Workspace};

    #[test]
    fn ui_state_applies_stream_events() {
        let mut s = UiState::default();
        let mut c = Conversation::new(Workspace::scratch().id);
        c.messages.push(Message::user("hi"));
        let id = c.id;
        s.apply(Event::Conversation(c));
        s.apply(Event::StreamStarted(id));
        s.apply(Event::Delta {
            conversation: id,
            kind: DeltaKind::Reasoning,
            text: "hm".into(),
        });
        s.apply(Event::Delta {
            conversation: id,
            kind: DeltaKind::Text,
            text: "He".into(),
        });
        s.apply(Event::Delta {
            conversation: id,
            kind: DeltaKind::Text,
            text: "llo".into(),
        });
        assert_eq!(s.streams[&id].text, "Hello");
        assert_eq!(s.streams[&id].reasoning, "hm");
        let done = Message::new(
            Role::Assistant,
            vec![Part::Reasoning("hm".into()), Part::Text("Hello".into())],
        );
        s.apply(Event::StreamFinished {
            conversation: id,
            message: done.clone(),
            tokens_per_sec: Some(12.5),
        });
        assert!(s.streams.is_empty());
        assert_eq!(s.loaded[&id].messages.last(), Some(&done));
        assert_eq!(s.last_tps, Some(12.5));
    }

    #[test]
    fn new_empty_conversation_is_reported_once() {
        let mut s = UiState::default();
        let c = Conversation::new(Workspace::scratch().id);
        let id = c.id;
        s.apply(Event::Conversation(c.clone()));
        assert_eq!(s.take_created(), Some(id));
        assert_eq!(s.take_created(), None);
        s.apply(Event::Conversation(c));
        assert_eq!(s.take_created(), None);
    }

    #[test]
    fn errors_become_the_status() {
        let mut s = UiState::default();
        s.apply(Event::Error("boom".into()));
        assert_eq!(
            s.status.as_ref().map(|st| (st.text.as_str(), st.error)),
            Some(("boom", true))
        );
    }

    #[test]
    fn workspace_switch_keeps_layout_for_the_app() {
        let mut s = UiState::default();
        s.apply(Event::WorkspaceOpened {
            workspace: Workspace::scratch(),
            conversations: vec![],
            recent: vec![],
            layout: Some("{}".into()),
            warnings: vec![],
        });
        assert_eq!(s.take_layout(), Some(Some("{}".into())));
        assert_eq!(s.take_layout(), None);
    }
}
