//! Command-line arguments for one headless Claude Code turn.

use uuid::Uuid;

/// Whether the turn starts a new Claude session or continues one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionMode {
    New(Uuid),
    Resume(Uuid),
}

/// Everything that shapes the `claude` command line for a turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnArgs {
    pub model: Option<String>,
    pub plan_mode: bool,
    pub session: SessionMode,
    pub append_system: String,
}

/// Build the argv (without the program name). `--verbose` is required by the CLI for
/// stream-json output. `--system-prompt` is deliberately never passed so Claude Code
/// keeps its own default prompt.
pub fn build_args(t: &TurnArgs) -> Vec<String> {
    let mut a: Vec<String> = [
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--input-format",
        "stream-json",
        "--include-partial-messages",
        "--permission-prompts",
        "host",
        // Without it the CLI denies on its own instead of sending `can_use_tool` (2.1.330).
        "--permission-prompt-tool",
        "stdio",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if t.plan_mode {
        a.push("--permission-mode".into());
        a.push("plan".into());
    }
    if let Some(m) = &t.model {
        a.push("--model".into());
        a.push(m.clone());
    }
    a.push(match t.session {
        SessionMode::New(u) => format!("--session-id={u}"),
        SessionMode::Resume(u) => format!("--resume={u}"),
    });
    a.push("--append-system-prompt".into());
    a.push(t.append_system.clone());
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(session: SessionMode) -> TurnArgs {
        TurnArgs {
            model: None,
            plan_mode: false,
            session,
            append_system: "NOTE".into(),
        }
    }

    #[test]
    fn args_first_turn() {
        let id = Uuid::nil();
        let a = build_args(&base(SessionMode::New(id)));
        let want: Vec<String> = [
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--input-format",
            "stream-json",
            "--include-partial-messages",
            "--permission-prompts",
            "host",
            "--permission-prompt-tool",
            "stdio",
            &format!("--session-id={id}"),
            "--append-system-prompt",
            "NOTE",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(a, want);
        assert!(!a.iter().any(|x| x == "--system-prompt"));
    }

    #[test]
    fn args_resume_plan_model() {
        let id = Uuid::nil();
        let mut t = base(SessionMode::Resume(id));
        t.plan_mode = true;
        t.model = Some("opus".into());
        let a = build_args(&t);
        let pos = |s: &str| a.iter().position(|x| x == s).unwrap();
        assert_eq!(a[pos("--permission-mode") + 1], "plan");
        assert_eq!(a[pos("--model") + 1], "opus");
        assert!(a.contains(&format!("--resume={id}")));
        assert!(!a.iter().any(|x| x.starts_with("--session-id")));
        assert_eq!(a.last().unwrap(), "NOTE");
        assert!(a.contains(&"--verbose".to_string()));
    }
}
