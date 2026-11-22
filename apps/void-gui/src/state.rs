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

/// A tool call as it runs (live output) and once it's finished.
#[derive(Clone, Debug)]
pub struct ToolCallView {
    pub summary: String,
    pub output: String,
    pub done: Option<void_core::tools::ToolOutput>,
}

const LIVE_OUTPUT_CAP: usize = 200_000;

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
    pub tool_calls: HashMap<String, ToolCallView>,
    pub approvals: Vec<void_core::permissions::ApprovalRequest>,
    pub sandbox: Option<void_core::sandbox::Availability>,
    pub changes: HashMap<ConversationId, Vec<void_core::overlay::Change>>,
    pub diffs: HashMap<(ConversationId, std::path::PathBuf), Vec<void_core::overlay::Hunk>>,
    /// Bumped whenever diffs are dropped (so panels know to ask again).
    pub diffs_epoch: u64,
    /// The first pending changes of a chat ask the app to show Review.
    review_request: bool,
    reviewed: std::collections::HashSet<ConversationId>,
    /// Set by WorkspaceOpened: the layout the app should apply (None = default).
    layout: Option<Option<String>>,
    /// A conversation the engine just created, for the app to put in a Chat tab.
    created: Option<ConversationId>,
    /// Conversations deleted since the app last looked (their tabs need clearing).
    deleted: Vec<ConversationId>,
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
            tool_calls: HashMap::new(),
            approvals: Vec::new(),
            sandbox: None,
            changes: HashMap::new(),
            diffs: HashMap::new(),
            diffs_epoch: 0,
            review_request: false,
            reviewed: Default::default(),
            layout: None,
            created: None,
            deleted: Vec::new(),
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
            Event::ConversationCreated(c) => {
                self.created = Some(c.id);
                self.loaded.insert(c.id, c);
            }
            Event::Conversation(c) => {
                self.loaded.insert(c.id, c);
            }
            Event::ConversationDeleted(id) => {
                self.loaded.remove(&id);
                self.streams.remove(&id);
                self.deleted.push(id);
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
            Event::Info(m) => self.info(m),
            Event::SandboxStatus(a) => self.sandbox = Some(a),
            Event::ToolStarted {
                call_id, summary, ..
            } => {
                self.tool_calls.insert(
                    call_id,
                    ToolCallView {
                        summary,
                        output: String::new(),
                        done: None,
                    },
                );
            }
            Event::ToolOutput { call_id, text, .. } => {
                if let Some(t) = self.tool_calls.get_mut(&call_id) {
                    t.output.push_str(&text);
                    if t.output.len() > LIVE_OUTPUT_CAP {
                        let mut cut = t.output.len() - LIVE_OUTPUT_CAP / 2;
                        while !t.output.is_char_boundary(cut) {
                            cut += 1;
                        }
                        t.output.drain(..cut);
                    }
                }
            }
            Event::ToolFinished {
                conversation,
                call_id,
                output,
            } => {
                if let Some(c) = self.loaded.get_mut(&conversation) {
                    c.messages.push(void_core::model::Message::new(
                        void_core::model::Role::Tool,
                        vec![void_core::model::Part::ToolResult {
                            call_id: call_id.clone(),
                            content: output.content.clone(),
                        }],
                    ));
                }
                if let Some(t) = self.tool_calls.get_mut(&call_id) {
                    t.done = Some(output);
                }
            }
            Event::ApprovalNeeded(req) => self.approvals.push(req),
            Event::ApprovalResolved { id } => self.approvals.retain(|a| a.id != id),
            Event::Changes {
                conversation,
                changes,
            } => {
                if !changes.is_empty() && self.reviewed.insert(conversation) {
                    self.review_request = true;
                }
                self.changes.insert(conversation, changes);
                self.diffs.retain(|(c, _), _| *c != conversation);
                self.diffs_epoch += 1;
            }
            Event::Diff {
                conversation,
                path,
                hunks,
            } => {
                self.diffs.insert((conversation, path), hunks);
            }
            Event::ReviewError { error, .. } => self.error(error),
            Event::Compacted(_) => self.info("Earlier messages summarised to save context."),
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

    pub fn take_review_request(&mut self) -> bool {
        std::mem::take(&mut self.review_request)
    }

    pub fn take_created(&mut self) -> Option<ConversationId> {
        self.created.take()
    }

    pub fn take_deleted(&mut self) -> Vec<ConversationId> {
        std::mem::take(&mut self.deleted)
    }

    /// Apply one change to the saved config (and the draft), returning the config to persist,
    /// so unsaved Settings edits aren't saved along with it.
    pub fn save_change(&mut self, change: impl Fn(&mut Config)) -> Config {
        change(&mut self.saved_config);
        change(&mut self.config);
        self.saved_config.clone()
    }

    /// The profile a conversation uses (its own, else the saved active one): what the engine
    /// will actually send to, whatever unsaved Settings say.
    pub fn profile_for(&self, id: Option<ConversationId>) -> Option<&void_core::config::Profile> {
        let pid = id
            .and_then(|id| self.loaded.get(&id))
            .and_then(|c| c.profile_id.clone())
            .unwrap_or_else(|| self.saved_config.active_profile.clone());
        self.saved_config.profile(&pid)
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
    fn only_created_conversations_are_reported() {
        let mut s = UiState::default();
        let old = Conversation::new(Workspace::scratch().id);
        // An empty conversation from an earlier session, updated (e.g. renamed): not new.
        s.apply(Event::Conversation(old));
        assert_eq!(s.take_created(), None);
        let c = Conversation::new(Workspace::scratch().id);
        let id = c.id;
        s.apply(Event::ConversationCreated(c));
        assert_eq!(s.take_created(), Some(id));
        assert_eq!(s.take_created(), None);
    }

    #[test]
    fn deleted_conversation_is_forgotten() {
        let mut s = UiState::default();
        let c = Conversation::new(Workspace::scratch().id);
        let id = c.id;
        s.apply(Event::ConversationCreated(c));
        s.apply(Event::StreamStarted(id));
        s.apply(Event::ConversationDeleted(id));
        assert!(!s.loaded.contains_key(&id));
        assert!(!s.streams.contains_key(&id));
        assert_eq!(s.take_deleted(), vec![id]);
    }

    #[test]
    fn unsaved_settings_do_not_change_the_backend_in_use() {
        let mut s = UiState::default();
        s.config.active_profile = "ollama".into();
        assert_eq!(s.profile_for(None).unwrap().id, "strata");
    }

    #[test]
    fn saving_one_change_leaves_other_drafts_unsaved() {
        let mut s = UiState::default();
        s.config.base_system_prompt = "half-typed".into();
        let sent = s.save_change(|c| c.active_profile = "ollama".into());
        assert_eq!(sent.active_profile, "ollama");
        assert_ne!(sent.base_system_prompt, "half-typed");
        assert_eq!(s.saved_config, sent);
        assert_eq!(s.config.active_profile, "ollama");
        assert_eq!(s.config.base_system_prompt, "half-typed");
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

    #[test]
    fn ui_state_tracks_tool_calls() {
        let mut s = UiState::default();
        let c = Conversation::new(Workspace::scratch().id);
        let id = c.id;
        s.apply(Event::Conversation(c));
        s.apply(Event::ToolStarted {
            conversation: id,
            call_id: "c1".into(),
            name: "bash".into(),
            summary: "ls".into(),
        });
        s.apply(Event::ToolOutput {
            conversation: id,
            call_id: "c1".into(),
            text: "a\n".into(),
        });
        s.apply(Event::ToolOutput {
            conversation: id,
            call_id: "c1".into(),
            text: "b\n".into(),
        });
        assert_eq!(s.tool_calls["c1"].output, "a\nb\n");
        assert!(s.tool_calls["c1"].done.is_none());
        let out = void_core::tools::ToolOutput::ok("exit code: 0\na\nb");
        s.apply(Event::ToolFinished {
            conversation: id,
            call_id: "c1".into(),
            output: out.clone(),
        });
        assert_eq!(s.tool_calls["c1"].done, Some(out));
        // The result is mirrored into the conversation like the engine stores it.
        let last = s.loaded[&id].messages.last().unwrap();
        assert_eq!(last.role, Role::Tool);
    }

    #[test]
    fn approval_added_and_resolved() {
        let mut s = UiState::default();
        let req = void_core::permissions::ApprovalRequest {
            id: 7,
            conversation: void_core::model::ConversationId::new(),
            tool: "bash".into(),
            summary: "rm -rf build".into(),
            command: Some("rm -rf build".into()),
        };
        s.apply(Event::ApprovalNeeded(req.clone()));
        assert_eq!(s.approvals, vec![req]);
        s.apply(Event::ApprovalResolved { id: 7 });
        assert!(s.approvals.is_empty());
    }

    #[test]
    fn first_pending_changes_ask_for_review() {
        let mut s = UiState::default();
        let id = void_core::model::ConversationId::new();
        let change = void_core::overlay::Change {
            path: "a.txt".into(),
            kind: void_core::overlay::ChangeKind::Added,
            ignored: false,
            conflict: false,
        };
        s.apply(Event::Changes {
            conversation: id,
            changes: vec![],
        });
        assert!(!s.take_review_request());
        s.apply(Event::Changes {
            conversation: id,
            changes: vec![change.clone()],
        });
        assert!(s.take_review_request());
        s.apply(Event::Changes {
            conversation: id,
            changes: vec![change],
        });
        assert!(!s.take_review_request(), "only the first time");
        assert_eq!(s.changes[&id].len(), 1);
    }

    #[test]
    fn changes_drop_stale_diffs() {
        let mut s = UiState::default();
        let id = void_core::model::ConversationId::new();
        s.apply(Event::Diff {
            conversation: id,
            path: "a".into(),
            hunks: vec![],
        });
        let epoch = s.diffs_epoch;
        s.apply(Event::Changes {
            conversation: id,
            changes: vec![],
        });
        assert!(s.diffs.is_empty());
        assert!(s.diffs_epoch > epoch);
    }
}
