//! Which tool calls run freely, need the user's approval, or are refused — and the approval hook
//! a future auditor model can answer instead of him.

use std::path::Path;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::fsutil;
use crate::model::ConversationId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Allow,
    Ask,
    Deny,
}

/// `tool` (optionally only when the command / path starts with `prefix`) → `action`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    pub action: Action,
}

/// A workspace's rules (`workspaces/<id>/permissions.json`). The last matching rule wins.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permissions {
    pub rules: Vec<Rule>,
}

fn starts_with_word(s: &str, prefix: &str) -> bool {
    s.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

/// Shell syntax that chains or redirects: a prefix rule never covers what follows it.
fn chains(command: &str) -> bool {
    [";", "&", "|", "`", "$(", "\n", "<", ">"]
        .iter()
        .any(|t| command.contains(t))
}

impl Permissions {
    /// Reads and edits run freely (edits only touch the overlay); bash asks.
    pub fn defaults() -> Permissions {
        let rule = |tool: &str, action| Rule {
            tool: tool.into(),
            prefix: None,
            action,
        };
        Permissions {
            rules: vec![
                rule("read", Action::Allow),
                rule("list", Action::Allow),
                rule("glob", Action::Allow),
                rule("grep", Action::Allow),
                rule("edit", Action::Allow),
                rule("write", Action::Allow),
                rule("bash", Action::Ask),
            ],
        }
    }

    pub fn decide(&self, tool: &str, args: &Value) -> Action {
        let subject = args
            .get("command")
            .or_else(|| args.get("path"))
            .and_then(Value::as_str)
            .map(str::trim_start)
            .unwrap_or("");
        self.rules
            .iter()
            .rev()
            .find(|r| {
                r.tool == tool
                    && r.prefix.as_deref().is_none_or(|p| {
                        starts_with_word(subject, p) && !(tool == "bash" && chains(subject))
                    })
            })
            .map(|r| r.action)
            .unwrap_or(Action::Ask)
    }

    /// "Always allow" for this workspace.
    pub fn add_always(&mut self, tool: &str, prefix: Option<String>) {
        self.rules.push(Rule {
            tool: tool.into(),
            prefix,
            action: Action::Allow,
        });
    }

    /// Missing → defaults; unreadable → set aside, defaults, and a warning.
    pub fn load(path: &Path) -> (Permissions, Option<String>) {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => return (Permissions::defaults(), None),
        };
        match serde_json::from_slice(&bytes) {
            Ok(p) => (p, None),
            Err(e) => {
                let kept = fsutil::set_aside(path)
                    .map(|b| b.display().to_string())
                    .unwrap_or_default();
                (
                    Permissions::defaults(),
                    Some(format!(
                        "The workspace's permissions file was unreadable ({e}); moved to {kept}."
                    )),
                )
            }
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        fsutil::atomic_write(
            path,
            &serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?,
        )
    }
}

/// The command prefix an "Always allow" button offers: two words for multi-command tools
/// (`cargo test`, `git status`), one word otherwise.
pub fn always_prefix(command: &str) -> String {
    let words: Vec<&str> = command.split_whitespace().collect();
    let two = matches!(
        words.first(),
        Some(&("cargo" | "git" | "npm" | "pnpm" | "yarn" | "moon" | "proto" | "go" | "uv" | "pip"))
    );
    let n = if two { 2 } else { 1 };
    words.into_iter().take(n).collect::<Vec<_>>().join(" ")
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: u64,
    pub conversation: ConversationId,
    pub tool: String,
    pub summary: String,
    /// The full command, for bash.
    pub command: Option<String>,
    /// Whose request this is: void's own agent, or Claude Code (a Claude Code chat).
    #[serde(default)]
    pub kind: ApprovalKind,
}

/// Who is asking. void's own requests are decided by void's workspace rules; Claude Code's come
/// from Claude Code's own permission settings, and void's rules are never read or written for
/// them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalKind {
    #[default]
    Void,
    ClaudeCode {
        /// Claude Code suggested a permission update for "Always allow"
        /// ([`Decision::AlwaysAsSuggested`]); offer the button only then.
        always_allow: bool,
        /// Claude Code's own wording for the request, when it sent any.
        title: Option<String>,
        description: Option<String>,
        /// The path that made Claude Code ask (e.g. outside its allowed directories).
        blocked_path: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    Once,
    /// Allow now and add a rule for this command prefix (or the whole tool when empty).
    /// void's own requests only: for Claude Code's it allows once and writes no rule.
    AlwaysForWorkspace(String),
    /// Claude Code's "Always allow": allow and hand Claude Code back its own suggested
    /// permission update (Claude Code applies it; void writes nothing). Allows once when there
    /// was no suggestion, and for void's own requests.
    AlwaysAsSuggested,
    Deny,
}

/// Answers "ask" decisions. The engine's own approver asks the UI; an auditor model can
/// implement this later for auto mode.
pub trait Approver: Send + Sync {
    fn decide(&self, req: ApprovalRequest) -> BoxFuture<'static, Decision>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_allow_reads_and_edits_ask_bash() {
        let p = Permissions::defaults();
        for t in ["read", "list", "glob", "grep", "edit", "write"] {
            assert_eq!(p.decide(t, &json!({})), Action::Allow, "{t}");
        }
        assert_eq!(p.decide("bash", &json!({"command": "ls"})), Action::Ask);
        assert_eq!(p.decide("unknown", &json!({})), Action::Ask);
    }

    #[test]
    fn last_matching_rule_wins() {
        let mut p = Permissions::defaults();
        p.rules.push(Rule {
            tool: "edit".into(),
            prefix: None,
            action: Action::Deny,
        });
        assert_eq!(p.decide("edit", &json!({"path": "a"})), Action::Deny);
        p.rules.push(Rule {
            tool: "edit".into(),
            prefix: None,
            action: Action::Ask,
        });
        assert_eq!(p.decide("edit", &json!({"path": "a"})), Action::Ask);
    }

    #[test]
    fn bash_prefix_matches_word_boundary() {
        let mut p = Permissions::defaults();
        p.add_always("bash", Some("cargo test".into()));
        assert_eq!(
            p.decide("bash", &json!({"command": "cargo test -p x"})),
            Action::Allow
        );
        assert_eq!(
            p.decide("bash", &json!({"command": "cargo test"})),
            Action::Allow
        );
        assert_eq!(
            p.decide("bash", &json!({"command": "cargo testify"})),
            Action::Ask
        );
        assert_eq!(
            p.decide("bash", &json!({"command": "rm -rf ."})),
            Action::Ask
        );
    }

    #[test]
    fn always_prefix_choice() {
        assert_eq!(always_prefix("cargo test -p x"), "cargo test");
        assert_eq!(always_prefix("  git status --short"), "git status");
        assert_eq!(always_prefix("moon run :test"), "moon run");
        assert_eq!(always_prefix("ls -la"), "ls");
        assert_eq!(always_prefix("cargo"), "cargo");
    }

    #[test]
    fn save_load_roundtrip_and_corrupt_set_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");
        let (missing, w) = Permissions::load(&path);
        assert_eq!(missing, Permissions::defaults());
        assert!(w.is_none());
        let mut p = Permissions::defaults();
        p.add_always("bash", Some("ls".into()));
        p.save(&path).unwrap();
        assert_eq!(Permissions::load(&path).0, p);
        std::fs::write(&path, "{broken").unwrap();
        let (back, warning) = Permissions::load(&path);
        assert_eq!(back, Permissions::defaults());
        assert!(warning.is_some());
        assert!(
            std::fs::read_dir(dir.path()).unwrap().any(|e| e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".bad-"))
        );
    }

    #[test]
    fn chained_commands_dont_match_prefix_rules() {
        let mut p = Permissions::defaults();
        p.add_always("bash", Some("cargo test".into()));
        for cmd in [
            "cargo test; curl evil.example",
            "cargo test && rm -rf .",
            "cargo test | sh",
            "cargo test `curl x`",
            "cargo test $(curl x)",
            "cargo test\ncurl x",
            "cargo test > /tmp/x",
        ] {
            assert_eq!(
                p.decide("bash", &json!({"command": cmd})),
                Action::Ask,
                "{cmd}"
            );
        }
        assert_eq!(
            p.decide("bash", &json!({"command": "cargo test -p void-core"})),
            Action::Allow
        );
    }
}
