//! What Claude Code's tool calls show on their chat cards. Claude Code names its tools
//! (`Bash`, `Edit`, `Task`, `mcp__server__tool`…); void's own tools are lowercase, so a name is
//! enough to tell them apart, even in a chat that switched between kinds of profile.

use std::path::Path;

use serde_json::Value;
use void_core::claude_code::permission::display_path;

/// Lines of an edit's preview shown on its card.
pub const PREVIEW_LINES: usize = 8;
/// Characters of a one-line summary.
const SUMMARY_CHARS: usize = 120;

/// A tool call from Claude Code rather than void's own agent.
pub fn is_claude_tool(name: &str) -> bool {
    name.starts_with("mcp__") || name.chars().next().is_some_and(char::is_uppercase)
}

/// The card's title: MCP tools as "server · tool", Claude Code's own tools by name.
pub fn display_name(name: &str) -> String {
    match name
        .strip_prefix("mcp__")
        .and_then(|rest| rest.split_once("__"))
    {
        Some((server, tool)) if !server.is_empty() && !tool.is_empty() => {
            format!("{server} · {tool}")
        }
        _ => name.to_string(),
    }
}

/// A subagent's tool: its work streams onto this card.
pub fn is_subagent(name: &str) -> bool {
    matches!(name, "Task" | "Agent")
}

/// Tools whose card shows the file change itself.
pub fn is_edit(name: &str) -> bool {
    matches!(name, "Edit" | "MultiEdit" | "Write" | "NotebookEdit")
}

/// Tools with a card of their own (the rest get the generic card, arguments included).
fn is_known(name: &str) -> bool {
    is_edit(name)
        || is_subagent(name)
        || matches!(
            name,
            "Bash" | "Read" | "Glob" | "Grep" | "WebFetch" | "WebSearch" | "TodoWrite"
        )
}

/// The generic card's arguments: for tools without a card of their own.
pub fn shows_arguments(name: &str) -> bool {
    !is_known(name)
}

fn one_line(s: &str) -> String {
    let line = s.lines().next().unwrap_or("");
    let mut out: String = line.chars().take(SUMMARY_CHARS).collect();
    if line.chars().count() > SUMMARY_CHARS || s.lines().nth(1).is_some() {
        out.push('…');
    }
    out
}

/// The card's one-line summary from the call's arguments; `live` (the engine's summary from
/// `ToolStarted`) when the arguments aren't known. Paths are workspace-relative when inside
/// `root`, else absolute.
pub fn summary(name: &str, arguments: &str, live: &str, root: Option<&Path>) -> String {
    let v: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let field = |k: &str| v.get(k).and_then(Value::as_str);
    let path = |k: &str| field(k).map(|p| display_path(p, root));
    let known = match name {
        "Bash" => field("command").map(one_line),
        "Edit" | "MultiEdit" | "Write" | "Read" => path("file_path"),
        "NotebookEdit" => path("notebook_path").or_else(|| path("file_path")),
        "Glob" | "Grep" => field("pattern").map(|p| match path("path") {
            Some(dir) => format!("{p} in {dir}"),
            None => p.to_string(),
        }),
        "WebFetch" => field("url").map(str::to_string),
        "WebSearch" => field("query").map(str::to_string),
        "Task" | "Agent" => field("description")
            .or_else(|| field("prompt"))
            .map(one_line),
        "TodoWrite" => v.get("todos").and_then(Value::as_array).map(|t| {
            let done = t
                .iter()
                .filter(|x| x.get("status").and_then(Value::as_str) == Some("completed"))
                .count();
            format!("{} todos, {done} done", t.len())
        }),
        _ if v.is_object() => Some(one_line(&v.to_string())),
        _ => None,
    };
    match known {
        Some(s) => s,
        None if is_edit(name) || name == "Read" => display_path(live, root),
        None => live.to_string(),
    }
}

/// The arguments as pretty JSON (the generic card's expanded view).
pub fn pretty_arguments(arguments: &str) -> String {
    match serde_json::from_str::<Value>(arguments) {
        Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| arguments.to_string()),
        Err(_) => arguments.to_string(),
    }
}

/// An edit's change as diff lines (`-` removed, `+` added), all of it: the card shows the
/// first [`PREVIEW_LINES`].
pub fn edit_preview(name: &str, arguments: &str) -> Vec<(char, String)> {
    let v: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let mut out = Vec::new();
    let mut change = |old: &str, new: &str| {
        out.extend(old.lines().map(|l| ('-', l.to_string())));
        out.extend(new.lines().map(|l| ('+', l.to_string())));
    };
    match name {
        "Edit" => change(&text(&v, "old_string"), &text(&v, "new_string")),
        "MultiEdit" => {
            for e in v
                .get("edits")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                change(&text(e, "old_string"), &text(e, "new_string"));
            }
        }
        "Write" => change("", &text(&v, "content")),
        "NotebookEdit" => change("", &text(&v, "new_source")),
        _ => {}
    }
    out
}

/// A subagent's streamed work: how many tool calls it made (each a `› name …` line).
pub fn subagent_steps(live: &str) -> usize {
    live.lines().filter(|l| l.starts_with("› ")).count()
}

/// The collapsed header over a subagent's work.
pub fn subagent_header(live: &str) -> String {
    match subagent_steps(live) {
        0 => "subagent".into(),
        1 => "subagent · 1 step".into(),
        n => format!("subagent · {n} steps"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_names() {
        assert!(is_claude_tool("Bash") && is_claude_tool("mcp__github__create_issue"));
        assert!(!is_claude_tool("bash") && !is_claude_tool("edit"));
        assert_eq!(
            display_name("mcp__github__create_issue"),
            "github · create_issue"
        );
        assert_eq!(display_name("mcp__my_srv__do__it"), "my_srv · do__it");
        assert_eq!(display_name("mcp__broken"), "mcp__broken");
        assert_eq!(display_name("Bash"), "Bash");
    }

    #[test]
    fn summaries() {
        let root = Some(Path::new("/w"));
        let s = |name: &str, args: Value| summary(name, &args.to_string(), "", root);
        assert_eq!(s("Bash", json!({"command": "ls\npwd"})), "ls…");
        assert_eq!(s("Edit", json!({"file_path": "/w/src/a.rs"})), "src/a.rs");
        assert_eq!(s("Write", json!({"file_path": "/w/../x"})), "/x");
        assert_eq!(
            s("Grep", json!({"pattern": "fn", "path": "/w/src"})),
            "fn in src"
        );
        assert_eq!(s("Task", json!({"description": "Find it"})), "Find it");
        assert_eq!(
            s(
                "TodoWrite",
                json!({"todos": [{"status": "completed"}, {"status": "pending"}]})
            ),
            "2 todos, 1 done"
        );
        assert_eq!(s("mcp__a__b", json!({"k": 1})), r#"{"k":1}"#);
        // Live, before the arguments are known.
        assert_eq!(summary("Read", "", "/w/a.rs", root), "a.rs");
        assert_eq!(summary("Bash", "", "cd .. && ls", root), "cd .. && ls");
    }

    #[test]
    fn previews() {
        let args = json!({"edits": [
            {"old_string": "a", "new_string": "b\nc"},
            {"old_string": "d", "new_string": ""},
        ]});
        assert_eq!(
            edit_preview("MultiEdit", &args.to_string()),
            [
                ('-', "a".into()),
                ('+', "b".into()),
                ('+', "c".into()),
                ('-', "d".into())
            ]
        );
        let w = json!({"file_path": "x", "content": "one\ntwo\n"}).to_string();
        assert_eq!(
            edit_preview("Write", &w),
            [('+', "one".into()), ('+', "two".into())]
        );
        assert!(edit_preview("Bash", "{}").is_empty());
    }

    #[test]
    fn subagent_work() {
        let live = "› Grep fn\nsome text\n› Read a.rs\n";
        assert_eq!(subagent_header(live), "subagent · 2 steps");
        assert_eq!(subagent_header("› Read a\n"), "subagent · 1 step");
        assert_eq!(subagent_header("thinking"), "subagent");
    }
}
