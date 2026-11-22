//! Turning a conversation into a backend request, and counting how much context it uses.

use crate::backend::ChatRequest;
use crate::config::{Config, Profile};
use crate::model::{Conversation, Message, Part, Role};

fn text_only(m: &Message) -> bool {
    m.parts
        .iter()
        .all(|p| matches!(p, Part::Text(_) | Part::Reasoning(_)))
}

/// The request for the conversation's next reply. Failed replies, and stopped ones that never
/// produced text, are left out.
pub fn build_request(conv: &Conversation, profile: &Profile, cfg: &Config) -> ChatRequest {
    let messages = conv
        .messages
        .iter()
        .filter(|m| m.error.is_none())
        .filter(|m| {
            m.role != Role::Assistant
                || m.parts.iter().any(|p| {
                    matches!(p, Part::Text(t) if !t.is_empty())
                        || matches!(p, Part::ToolCall { .. })
                })
        })
        .cloned()
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
    }
}

/// A rough token count: one token per four characters, rounded up.
pub fn estimate_tokens(text: &str) -> u32 {
    text.chars().count().div_ceil(4) as u32
}

/// Tokens the conversation occupies: the last reported usage plus estimates for what came
/// after it, or estimates for everything when no backend reported usage.
pub fn context_used(conv: &Conversation) -> u32 {
    let last_usage = conv.messages.iter().rposition(|m| m.usage.is_some());
    let (base, rest) = match last_usage {
        Some(i) => {
            let u = conv.messages[i].usage.unwrap_or_default();
            (
                u.prompt_tokens + u.completion_tokens,
                &conv.messages[i + 1..],
            )
        }
        None => (0, &conv.messages[..]),
    };
    base + rest.iter().map(|m| estimate_tokens(&m.text())).sum::<u32>()
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
}
