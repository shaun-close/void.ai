//! Claude Code's permission prompts in void's approval dialog. Claude Code's own permission
//! settings decide what needs asking (`can_use_tool`); void shows the question as an ordinary
//! approval and writes the answer back on `claude`'s stdin. void's workspace rules are never
//! consulted for these chats, and void never writes them or Claude Code's settings: "Always
//! allow" hands Claude Code back its own suggested permission update, unchanged.
//!
//! The engine owns the pending prompts (they sit with void's own approvals, so Stop, Delete and
//! the turn's end close them the same way); [`HostBridge`] only carries requests to it.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use super::turn::{PermissionAsk, PermissionBridge, StdinTx};
use super::{control_response_allow, control_response_deny};
use crate::model::ConversationId;
use crate::permissions::{ApprovalKind, ApprovalRequest, Decision};

/// Claude Code is told this when the user picks Deny.
pub const DENIED: &str = "The user denied this in void.ai";
/// The answer to a request that arrives after its turn was stopped or ended.
pub const TURN_OVER: &str = "The turn was stopped in void.ai";
/// A summary of an unfamiliar tool's input, at most this many bytes.
const SUMMARY_CAP: usize = 2048;

/// A request on its way to the engine.
pub enum HostMsg {
    Ask {
        conversation: ConversationId,
        ask: PermissionAsk,
        stdin: StdinTx,
    },
    Cancel {
        conversation: ConversationId,
        request_id: String,
    },
}

/// The engine's bridge: hands requests to the engine, which shows them in the approval dialog
/// and answers on `stdin`.
pub struct HostBridge {
    /// False when the engine is gone (the request is then refused, so `claude` never waits).
    send: Box<dyn Fn(HostMsg) -> bool + Send + Sync>,
}

impl HostBridge {
    pub fn new(send: impl Fn(HostMsg) -> bool + Send + Sync + 'static) -> Self {
        Self {
            send: Box::new(send),
        }
    }
}

impl PermissionBridge for HostBridge {
    fn ask(&self, conversation: ConversationId, ask: PermissionAsk, stdin: StdinTx) {
        let (id, out) = (ask.request_id.clone(), stdin.clone());
        let sent = (self.send)(HostMsg::Ask {
            conversation,
            ask,
            stdin,
        });
        if !sent {
            let _ = out.send(control_response_deny(&id, TURN_OVER, false));
        }
    }

    fn cancel(&self, conversation: ConversationId, request_id: &str) {
        (self.send)(HostMsg::Cancel {
            conversation,
            request_id: request_id.to_string(),
        });
    }

    // turn_ended: the engine closes the chat's prompts itself wherever it reports the end.
}

/// Whether Claude Code suggested a permission update to apply for "Always allow".
pub fn offers_always(suggestions: Option<&Value>) -> bool {
    suggestions
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty())
}

/// `path` with `.` and `..` resolved lexically (symlinks aren't followed: this is for display).
/// A `..` that would climb above the root stays at the root; one that leads a relative path is
/// kept.
fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(".."),
            },
            c => out.push(c),
        }
    }
    out
}

/// A path as the approval dialog and tool cards show it: relative to the workspace (`base`)
/// when it's inside it, else absolute. Normalised first, so a path that leaves the workspace
/// through `..` shows where it really goes rather than as `../…` or under the workspace.
pub fn display_path(path: &str, base: Option<&Path>) -> String {
    let full = normalise(Path::new(path));
    match base.and_then(|b| full.strip_prefix(normalise(b)).ok()) {
        Some(rel) if !rel.as_os_str().is_empty() => rel.display().to_string(),
        _ => full.display().to_string(),
    }
}

/// `s` cut to at most `max` bytes (on a char boundary), with "…" when cut.
fn cap(s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// What the request is about, readably: the command, file, URL, query or pattern; else the
/// input as JSON. Paths are shown by [`display_path`].
pub fn summary(tool: &str, input: &Value, base: Option<&Path>) -> String {
    let field = |k: &str| input.get(k).and_then(Value::as_str);
    let path = |k: &str| field(k).map(|p| display_path(p, base));
    let known = match tool {
        "Bash" => field("command").map(str::to_string),
        "Edit" | "Write" | "MultiEdit" | "Read" => path("file_path"),
        "NotebookEdit" => path("notebook_path").or_else(|| path("file_path")),
        "WebFetch" => field("url").map(str::to_string),
        "WebSearch" => field("query").map(str::to_string),
        "Glob" | "Grep" => field("pattern").map(|p| match path("path") {
            Some(dir) => format!("{p} in {dir}"),
            None => p.to_string(),
        }),
        _ => None,
    };
    let text = known.unwrap_or_else(|| {
        serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string())
    });
    cap(text, SUMMARY_CAP)
}

/// The approval to show for Claude Code's request (`id` is set by the engine).
pub fn approval_request(
    conversation: ConversationId,
    ask: &PermissionAsk,
    base: Option<&Path>,
) -> ApprovalRequest {
    let command = (ask.tool == "Bash")
        .then(|| ask.input.get("command").and_then(Value::as_str))
        .flatten()
        .map(str::to_string);
    ApprovalRequest {
        id: 0,
        conversation,
        tool: ask.tool.clone(),
        summary: summary(&ask.tool, &ask.input, base),
        command,
        kind: ApprovalKind::ClaudeCode {
            always_allow: offers_always(ask.suggestions.as_ref()),
            title: ask.title.clone(),
            description: ask.description.clone(),
            blocked_path: ask.blocked_path.as_deref().map(|p| display_path(p, base)),
        },
    }
}

/// The stdin line answering `ask` with the user's decision. The input goes back unchanged;
/// "Always allow" sends Claude Code's own suggestions (only when it made some).
pub fn answer(ask: &PermissionAsk, decision: &Decision) -> String {
    let id = &ask.request_id;
    match decision {
        Decision::Deny => control_response_deny(id, DENIED, false),
        Decision::AlwaysAsSuggested if offers_always(ask.suggestions.as_ref()) => {
            control_response_allow(id, &ask.input, ask.suggestions.as_ref())
        }
        // Once; void's own "always" (never a rule for a Claude Code chat); nothing suggested.
        Decision::Once | Decision::AlwaysForWorkspace(_) | Decision::AlwaysAsSuggested => {
            control_response_allow(id, &ask.input, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_code::control_response_allow;
    use serde_json::json;

    fn ask(tool: &str, input: Value, suggestions: Option<Value>) -> PermissionAsk {
        PermissionAsk {
            request_id: "perm-1".into(),
            tool: tool.into(),
            input,
            suggestions,
            title: None,
            description: None,
            blocked_path: None,
        }
    }

    #[test]
    fn summaries_per_tool() {
        let ws = Path::new("/home/u/proj");
        let base = Some(ws);
        let cases = [
            (
                "Bash",
                json!({"command": "cargo test", "description": "x"}),
                "cargo test",
            ),
            (
                "Edit",
                json!({"file_path": "/home/u/proj/src/a.rs"}),
                "src/a.rs",
            ),
            (
                "Write",
                json!({"file_path": "/elsewhere/b.rs"}),
                "/elsewhere/b.rs",
            ),
            (
                "MultiEdit",
                json!({"file_path": "/home/u/proj/c.rs"}),
                "c.rs",
            ),
            (
                "NotebookEdit",
                json!({"notebook_path": "/home/u/proj/n.ipynb"}),
                "n.ipynb",
            ),
            (
                "Read",
                json!({"file_path": "/home/u/proj/README.md"}),
                "README.md",
            ),
            (
                "WebFetch",
                json!({"url": "https://example.com", "prompt": "p"}),
                "https://example.com",
            ),
            ("WebSearch", json!({"query": "rust egui"}), "rust egui"),
            ("Glob", json!({"pattern": "**/*.rs"}), "**/*.rs"),
            (
                "Grep",
                json!({"pattern": "fn main", "path": "/home/u/proj/src"}),
                "fn main in src",
            ),
        ];
        for (tool, input, want) in cases {
            assert_eq!(summary(tool, &input, base), want, "{tool}");
        }
        // Without a workspace (Scratch) paths stay as they are.
        assert_eq!(
            summary("Edit", &json!({"file_path": "/home/u/proj/a"}), None),
            "/home/u/proj/a"
        );
    }

    /// A path that leaves the workspace through `..` is shown absolute and normalised, never as
    /// a misleading `../…` relative path (or with the workspace prefix hiding where it goes).
    #[test]
    fn dotdot_paths_shown_absolute() {
        let base = Some(Path::new("/home/u/proj"));
        assert_eq!(
            display_path("/home/u/proj/../other/x.rs", base),
            "/home/u/other/x.rs"
        );
        assert_eq!(display_path("/home/u/proj/src/../a.rs", base), "a.rs");
        assert_eq!(display_path("/home/u/proj/./src/a.rs", base), "src/a.rs");
        assert_eq!(display_path("/a/../../etc/passwd", None), "/etc/passwd");
        assert_eq!(display_path("rel/../x", base), "x");
        assert_eq!(display_path("../x", base), "../x");
        assert_eq!(
            summary(
                "Edit",
                &json!({"file_path": "/home/u/proj/../.ssh/k"}),
                base
            ),
            "/home/u/.ssh/k"
        );
        let mut a = ask("Read", json!({"file_path": "/home/u/proj/a"}), None);
        a.blocked_path = Some("/home/u/proj/../secrets".into());
        match approval_request(ConversationId::new(), &a, base).kind {
            ApprovalKind::ClaudeCode { blocked_path, .. } => {
                assert_eq!(blocked_path.as_deref(), Some("/home/u/secrets"))
            }
            k => panic!("{k:?}"),
        }
    }

    #[test]
    fn other_tools_show_capped_json() {
        let input = json!({"server": "x", "args": {"a": 1}});
        assert_eq!(
            summary("mcp__x__do", &input, None),
            serde_json::to_string_pretty(&input).unwrap()
        );
        // A tool we know but without its usual field falls back to the JSON too.
        assert_eq!(summary("Bash", &json!({}), None), "{}");
        let big = json!({"data": "é".repeat(5000)});
        let s = summary("Other", &big, None);
        assert!(s.len() <= 2100, "{}", s.len());
        assert!(s.ends_with('…'));
    }

    #[test]
    fn request_carries_claude_details() {
        let c = ConversationId::new();
        let mut a = ask(
            "Bash",
            json!({"command": "rm -rf build"}),
            Some(json!([{"type": "addRules"}])),
        );
        a.title = Some("Claude wants to run rm".into());
        a.description = Some("Deletes build/".into());
        a.blocked_path = Some("/x".into());
        let r = approval_request(c, &a, None);
        assert_eq!(r.conversation, c);
        assert_eq!(r.tool, "Bash");
        assert_eq!(r.summary, "rm -rf build");
        assert_eq!(r.command.as_deref(), Some("rm -rf build"));
        assert_eq!(
            r.kind,
            ApprovalKind::ClaudeCode {
                always_allow: true,
                title: Some("Claude wants to run rm".into()),
                description: Some("Deletes build/".into()),
                blocked_path: Some("/x".into()),
            }
        );
        for none in [None, Some(json!([])), Some(json!({"type": "addRules"}))] {
            let r = approval_request(c, &ask("Read", json!({"file_path": "/a"}), none), None);
            assert!(
                matches!(
                    r.kind,
                    ApprovalKind::ClaudeCode {
                        always_allow: false,
                        ..
                    }
                ),
                "{:?}",
                r.kind
            );
            assert_eq!(r.command, None);
        }
    }

    #[test]
    fn engine_gone_denies() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let a = ask("Bash", json!({"command": "ls"}), None);
        HostBridge::new(|_| false).ask(ConversationId::new(), a, tx);
        assert_eq!(
            rx.try_recv().unwrap(),
            crate::claude_code::control_response_deny("perm-1", TURN_OVER, false)
        );
    }

    #[test]
    fn answers_per_decision() {
        let input = json!({"command": "rm -rf build"});
        let sug = json!([{"type": "addRules", "rules": [{"toolName": "Bash"}], "behavior": "allow", "destination": "session"}]);
        let a = ask("Bash", input.clone(), Some(sug.clone()));
        let once = control_response_allow("perm-1", &input, None);
        assert_eq!(answer(&a, &Decision::Once), once);
        assert_eq!(
            answer(&a, &Decision::AlwaysAsSuggested),
            control_response_allow("perm-1", &input, Some(&sug))
        );
        // void's own "always" never becomes a Claude Code (or void) rule.
        assert_eq!(answer(&a, &Decision::AlwaysForWorkspace("rm".into())), once);
        assert_eq!(
            answer(&a, &Decision::Deny),
            crate::claude_code::control_response_deny("perm-1", DENIED, false)
        );
        // Nothing to apply without a suggestion: allow once.
        let plain = ask("Bash", input.clone(), Some(json!([])));
        assert_eq!(answer(&plain, &Decision::AlwaysAsSuggested), once);
    }
}
