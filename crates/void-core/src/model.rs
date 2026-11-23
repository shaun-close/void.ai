//! The conversation data model shared by every backend and UI.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Identifies a conversation; uuid v7, so ids sort by creation time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConversationId(pub Uuid);

impl ConversationId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for ConversationId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ConversationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for ConversationId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(s).map(Self)
    }
}

/// Identifies a workspace: `"scratch"` or a hash of the folder's canonical path.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceId(pub String);

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// One piece of a message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Part {
    Text(String),
    Reasoning(String),
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    ToolResult {
        call_id: String,
        content: String,
    },
    /// Compaction marker: a summary of everything before it, sent in place of those messages.
    Summary(String),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
    pub created: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// The reply was cut short by Stop.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stopped: bool,
    /// The reply failed; this is the user-facing reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Message {
    pub fn new(role: Role, parts: Vec<Part>) -> Self {
        Self {
            role,
            parts,
            created: Utc::now(),
            usage: None,
            stopped: false,
            error: None,
        }
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self::new(Role::User, vec![Part::Text(text.into())])
    }

    /// All text parts, joined.
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| match p {
                Part::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    /// All reasoning parts, joined.
    pub fn reasoning(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| match p {
                Part::Reasoning(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    pub id: ConversationId,
    pub workspace_id: WorkspaceId,
    pub title: String,
    pub created: DateTime<Utc>,
    pub updated: DateTime<Utc>,
    /// The profile this conversation uses; None follows the active profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    /// The agent profile (build, plan…); None follows the default agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The Claude Code session this chat resumes (Claude Code profiles); None until its first
    /// Claude Code turn has started one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_session: Option<Uuid>,
    pub messages: Vec<Message>,
}

pub const NEW_CHAT_TITLE: &str = "New chat";

impl Conversation {
    pub fn new(workspace_id: WorkspaceId) -> Self {
        let now = Utc::now();
        Self {
            id: ConversationId::new(),
            workspace_id,
            title: NEW_CHAT_TITLE.into(),
            created: now,
            updated: now,
            profile_id: None,
            agent: None,
            claude_session: None,
            messages: Vec::new(),
        }
    }

    pub fn meta(&self) -> ConversationMeta {
        ConversationMeta {
            id: self.id,
            title: self.title.clone(),
            updated: self.updated,
            profile_id: self.profile_id.clone(),
        }
    }
}

/// What the conversation list needs without loading every message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationMeta {
    pub id: ConversationId,
    pub title: String,
    pub updated: DateTime<Utc>,
    pub profile_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    /// The project folder; None for Scratch.
    pub root: Option<PathBuf>,
}

pub const SCRATCH_ID: &str = "scratch";

impl Workspace {
    pub fn scratch() -> Self {
        Self {
            id: WorkspaceId(SCRATCH_ID.into()),
            name: "Scratch".into(),
            root: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_text_joins_text_parts() {
        let m = Message::new(
            Role::Assistant,
            vec![
                Part::Reasoning("r".into()),
                Part::Text("a".into()),
                Part::Text("b".into()),
            ],
        );
        assert_eq!(m.text(), "ab");
        assert_eq!(m.reasoning(), "r");
    }

    #[test]
    fn conversation_json_roundtrip() {
        let mut c = Conversation::new(Workspace::scratch().id);
        c.messages.push(Message::user("hi"));
        let json = serde_json::to_string(&c).unwrap();
        let back: Conversation = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
        assert_eq!(back.meta().title, "New chat");
    }

    #[test]
    fn summary_part_is_not_text() {
        let m = Message::new(
            Role::Assistant,
            vec![Part::Summary("s".into()), Part::Text("t".into())],
        );
        assert_eq!(m.text(), "t");
    }

    #[test]
    fn conversation_without_agent_field_loads() {
        let c = Conversation::new(Workspace::scratch().id);
        let mut v = serde_json::to_value(&c).unwrap();
        v.as_object_mut().unwrap().remove("agent");
        let back: Conversation = serde_json::from_value(v).unwrap();
        assert_eq!(back.agent, None);
    }

    #[test]
    fn conversation_without_claude_session_loads() {
        let mut c = Conversation::new(Workspace::scratch().id);
        let v = serde_json::to_value(&c).unwrap();
        assert!(v.get("claude_session").is_none(), "None isn't written");
        let back: Conversation = serde_json::from_value(v).unwrap();
        assert_eq!(back.claude_session, None);
        c.claude_session = Some(Uuid::now_v7());
        let back: Conversation = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back, c);
    }
}
