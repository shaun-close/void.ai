//! Turning a conversation into a backend request, and counting how much context it uses.

use crate::backend::ChatRequest;
use crate::config::{Config, Profile};
use crate::model::{Conversation, Message, Part, Role};

fn text_only(m: &Message) -> bool {
    !m.parts.is_empty()
        && m.parts
            .iter()
            .all(|p| matches!(p, Part::Text(_) | Part::Reasoning(_)))
}

/// The request for the conversation's next reply. Failed replies, and stopped ones that never
/// produced text, are left out.
/// Index of the last compaction marker.
fn summary_index(conv: &Conversation) -> Option<usize> {
    conv.messages
        .iter()
        .rposition(|m| m.parts.iter().any(|p| matches!(p, Part::Summary(_))))
}

fn summary_text(m: &Message) -> String {
    m.parts
        .iter()
        .filter_map(|p| match p {
            Part::Summary(s) => Some(s.as_str()),
            _ => None,
        })
        .collect()
}

pub fn build_request(conv: &Conversation, profile: &Profile, cfg: &Config) -> ChatRequest {
    let start = summary_index(conv);
    let mut source: Vec<Message> = Vec::new();
    if let Some(i) = start {
        source.push(Message::user(format!(
            "Summary of earlier conversation:\n{}",
            summary_text(&conv.messages[i])
        )));
    }
    let after = start.map(|i| i + 1).unwrap_or(0);
    source.extend(
        conv.messages[after..]
            .iter()
            .filter(|m| m.error.is_none())
            .filter(|m| {
                m.role != Role::Assistant
                    || m.parts.iter().any(|p| {
                        matches!(p, Part::Text(t) if !t.is_empty())
                            || matches!(p, Part::ToolCall { .. })
                    })
            })
            .cloned(),
    );
    let source = answer_dangling_calls(source);
    let messages = source
        .into_iter()
        .fold(Vec::<Message>::new(), |mut out, m| {
            // Strict chat templates need alternating turns: merge a run of one role's text.
            match out.last_mut() {
                Some(prev)
                    if prev.role == m.role
                        && matches!(m.role, Role::User | Role::Assistant)
                        && text_only(prev)
                        && text_only(&m) =>
                {
                    let joined = format!("{}\n\n{}", prev.text(), m.text());
                    prev.parts = vec![Part::Text(joined)];
                }
                _ => out.push(m),
            }
            out
        });
    ChatRequest {
        model: profile.model.clone(),
        system: cfg.base_system_prompt.clone(),
        messages,
        max_output: profile.max_output,
        extra_body: profile.extra_body.clone(),
        tools: Vec::new(),
    }
}

/// Every tool call needs a result before the next turn (APIs reject the history otherwise).
/// Calls cut off by Stop get a placeholder.
fn answer_dangling_calls(msgs: Vec<Message>) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(msgs.len());
    let mut pending: Vec<String> = Vec::new();
    let flush = |out: &mut Vec<Message>, pending: &mut Vec<String>| {
        for id in pending.drain(..) {
            out.push(Message::new(
                Role::Tool,
                vec![Part::ToolResult {
                    call_id: id,
                    content: "(not run: the turn was stopped)".into(),
                }],
            ));
        }
    };
    for m in msgs {
        match m.role {
            Role::Tool => {
                for p in &m.parts {
                    if let Part::ToolResult { call_id, .. } = p {
                        pending.retain(|id| id != call_id);
                    }
                }
                out.push(m);
            }
            _ => {
                flush(&mut out, &mut pending);
                if m.role == Role::Assistant {
                    pending.extend(m.parts.iter().filter_map(|p| match p {
                        Part::ToolCall { id, .. } => Some(id.clone()),
                        _ => None,
                    }));
                }
                out.push(m);
            }
        }
    }
    flush(&mut out, &mut pending);
    out
}

/// A rough token count: one token per four characters, rounded up.
pub fn estimate_tokens(text: &str) -> u32 {
    text.chars().count().div_ceil(4) as u32
}

/// Estimated tokens for what a message sends (text, tool calls and results, summaries).
fn message_tokens(m: &Message) -> u32 {
    m.parts
        .iter()
        .map(|p| match p {
            Part::Text(t) | Part::Summary(t) => estimate_tokens(t),
            Part::ToolCall {
                name, arguments, ..
            } => estimate_tokens(name) + estimate_tokens(arguments),
            Part::ToolResult { content, .. } => estimate_tokens(content),
            Part::Reasoning(_) => 0,
        })
        .sum()
}

/// Tokens the conversation occupies from its last summary on: the last reported usage plus
/// estimates for what came after it, or estimates for everything when no usage was reported.
pub fn context_used(conv: &Conversation) -> u32 {
    let from = summary_index(conv).unwrap_or(0);
    let msgs = &conv.messages[from..];
    let last_usage = msgs.iter().rposition(|m| m.usage.is_some());
    let (base, rest) = match last_usage {
        Some(i) => {
            let u = msgs[i].usage.unwrap_or_default();
            (u.prompt_tokens + u.completion_tokens, &msgs[i + 1..])
        }
        None => (0, msgs),
    };
    base + rest.iter().map(message_tokens).sum::<u32>()
}

/// Head-room kept free before compacting: opencode's 123 048 tokens, or half a small window.
pub fn compaction_buffer(context_limit: u32) -> u32 {
    123_048.min(context_limit / 2)
}

/// Whether the next request should compact first.
pub fn needs_compaction(conv: &Conversation, profile: &Profile) -> bool {
    context_used(conv)
        > profile
            .context_limit
            .saturating_sub(compaction_buffer(profile.context_limit))
}

/// Where to cut for compaction: everything before the last user message (None when there's
/// nothing to summarise).
pub fn compaction_split(conv: &Conversation) -> Option<usize> {
    let split = conv.messages.iter().rposition(|m| m.role == Role::User)?;
    let from = summary_index(conv).map(|i| i + 1).unwrap_or(0);
    (split > from).then_some(split)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::{Conversation, Message, Part, Role, Usage, Workspace};

    #[test]
    fn build_request_skips_failed_messages() {
        let cfg = Config::default();
        let profile = cfg.profile("strata").unwrap();
        let mut c = Conversation::new(Workspace::scratch().id);
        c.messages.push(Message::user("one"));
        let mut failed = Message::new(Role::Assistant, vec![Part::Text("partial".into())]);
        failed.error = Some("boom".into());
        c.messages.push(failed);
        let mut empty_stop = Message::new(Role::Assistant, vec![Part::Reasoning("r".into())]);
        empty_stop.stopped = true;
        c.messages.push(empty_stop);
        c.messages.push(Message::user("two"));
        let req = build_request(&c, profile, &cfg);
        let texts: Vec<_> = req.messages.iter().map(|m| m.text()).collect();
        // The failed and empty replies are left out; the two user turns then merge.
        assert_eq!(texts, ["one\n\ntwo"]);
        assert_eq!(req.system, cfg.base_system_prompt);
        assert_eq!(req.model, "qwen3.8-flash-next-iq3_xxs");
        assert_eq!(req.max_output, 32768);
        assert_eq!(req.extra_body, profile.extra_body);
    }

    #[test]
    fn adjacent_turns_of_one_role_are_merged() {
        let cfg = Config::default();
        let profile = cfg.profile("strata").unwrap();
        let mut c = Conversation::new(Workspace::scratch().id);
        c.messages.push(Message::user("first"));
        let mut failed = Message::new(Role::Assistant, vec![Part::Text("x".into())]);
        failed.error = Some("down".into());
        c.messages.push(failed);
        c.messages.push(Message::user("second"));
        let req = build_request(&c, profile, &cfg);
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, Role::User);
        assert_eq!(req.messages[0].text(), "first\n\nsecond");
    }

    #[test]
    fn estimate_tokens_rounds_up() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        assert_eq!(estimate_tokens("ééé"), 1);
    }

    #[test]
    fn context_used_prefers_reported_usage() {
        let mut c = Conversation::new(Workspace::scratch().id);
        c.messages.push(Message::user("abcdefgh"));
        assert_eq!(context_used(&c), 2);
        let mut a = Message::new(Role::Assistant, vec![Part::Text("x".into())]);
        a.usage = Some(Usage {
            prompt_tokens: 100,
            completion_tokens: 20,
        });
        c.messages.push(a);
        c.messages.push(Message::user("abcd"));
        assert_eq!(context_used(&c), 121);
    }

    #[test]
    fn tool_messages_are_not_merged() {
        let cfg = Config::default();
        let profile = cfg.profile("strata").unwrap();
        let mut c = Conversation::new(Workspace::scratch().id);
        c.messages.push(Message::user("go"));
        c.messages.push(Message::new(
            Role::Assistant,
            vec![Part::ToolCall {
                id: "c1".into(),
                name: "read".into(),
                arguments: "{}".into(),
            }],
        ));
        c.messages.push(Message::new(
            Role::Tool,
            vec![Part::ToolResult {
                call_id: "c1".into(),
                content: "A".into(),
            }],
        ));
        c.messages.push(Message::new(
            Role::Tool,
            vec![Part::ToolResult {
                call_id: "c2".into(),
                content: "B".into(),
            }],
        ));
        let req = build_request(&c, profile, &cfg);
        assert_eq!(req.messages.len(), 4);
        assert!(req.tools.is_empty());
    }

    #[test]
    fn summary_marker_replaces_earlier_messages() {
        let cfg = Config::default();
        let profile = cfg.profile("strata").unwrap();
        let mut c = Conversation::new(Workspace::scratch().id);
        c.messages.push(Message::user("old question"));
        c.messages.push(Message::new(
            Role::Assistant,
            vec![Part::Text("old answer".into())],
        ));
        c.messages.push(Message::new(
            Role::Assistant,
            vec![Part::Summary("we discussed X".into())],
        ));
        c.messages.push(Message::user("new question"));
        let req = build_request(&c, profile, &cfg);
        assert_eq!(req.messages.len(), 1);
        let t = req.messages[0].text();
        assert!(
            t.starts_with("Summary of earlier conversation:\nwe discussed X"),
            "{t}"
        );
        assert!(t.ends_with("new question"));
        assert!(!t.contains("old question"));
    }

    #[test]
    fn unanswered_tool_calls_get_placeholder_results() {
        let cfg = Config::default();
        let profile = cfg.profile("strata").unwrap();
        let mut c = Conversation::new(Workspace::scratch().id);
        c.messages.push(Message::user("go"));
        c.messages.push(Message::new(
            Role::Assistant,
            vec![
                Part::ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments: "{}".into(),
                },
                Part::ToolCall {
                    id: "c2".into(),
                    name: "bash".into(),
                    arguments: "{}".into(),
                },
            ],
        ));
        c.messages.push(Message::new(
            Role::Tool,
            vec![Part::ToolResult {
                call_id: "c1".into(),
                content: "A".into(),
            }],
        ));
        c.messages.push(Message::user("why did you stop?"));
        let req = build_request(&c, profile, &cfg);
        let results: Vec<_> = req
            .messages
            .iter()
            .flat_map(|m| m.parts.iter())
            .filter_map(|p| match p {
                Part::ToolResult { call_id, content } => Some((call_id.clone(), content.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 2);
        assert_eq!(results[1].0, "c2");
        assert!(results[1].1.contains("not run"));
        assert_eq!(req.messages.last().unwrap().text(), "why did you stop?");
    }

    #[test]
    fn needs_compaction_near_the_limit() {
        let cfg = Config::default();
        let mut profile = cfg.profile("strata").unwrap().clone();
        profile.context_limit = 200; // buffer = min(123048, 100) = 100 → compact above 100 tokens
        let mut c = Conversation::new(Workspace::scratch().id);
        c.messages.push(Message::user("x".repeat(300)));
        assert!(!needs_compaction(&c, &profile));
        c.messages.push(Message::user("y".repeat(200)));
        assert!(needs_compaction(&c, &profile));
        assert_eq!(compaction_buffer(262144), 123048);
    }
}
