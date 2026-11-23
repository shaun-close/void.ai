//! Chat turns through Claude Code, end to end through the engine and the real sandbox, with the
//! fake `claude` (tests/fake-claude): no network, no tokens. Everything lives under the build's
//! tmp dir with a dummy HOME (never the real one, whose Claude Code state stays hidden too).
//! Tests skip with a note when the sandbox isn't available.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use uuid::Uuid;
use void_core::config::{ApiKeySource, BackendKind, Config, Profile, save_config};
use void_core::engine::{Command, DeltaKind, Engine, EngineHandle, EngineOptions, Event};
use void_core::model::{Conversation, ConversationId, Message, Part, Role, Workspace};
use void_core::overlay::Overlay;
use void_core::sandbox::{Availability, SandboxSettings};
use void_core::store::Store;

const NOT_SIGNED_IN: &str = "Claude Code isn't signed in — run `claude` in a terminal to sign in";
const BUSY: &str = "A Claude Code turn is running — try again when it finishes";
const RETRY_NOTE: &str = "The previous attempt was undone; its file changes were reverted.";

fn tests_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests")
}

struct Rig {
    tmp: tempfile::TempDir,
    home: PathBuf,
    ws: PathBuf,
    store: Store,
    handle: EngineHandle,
}

struct Setup {
    /// Shell that sets `sc` (a scenario name) from `$n` (1 for the first spawn) and `$*`.
    pick: String,
    api_key: ApiKeySource,
    network: bool,
    command: Option<String>,
    /// Conversations saved before the engine starts (in the workspace).
    before: Vec<Conversation>,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            pick: "sc=text-turn".into(),
            api_key: ApiKeySource::None,
            network: true,
            command: None,
            before: Vec::new(),
        }
    }
}

/// The fake `claude`, wrapped so each spawn records its argv, stdin, cwd and key under the dummy
/// `~/.claude/fake/` (bound read-write, so the host sees it), numbered from 1.
fn wrapper(dir: &Path, pick: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let fake = tests_dir().join("fake-claude");
    let scenarios = tests_dir().join("fake-claude-scenarios");
    let script = format!(
        r#"#!/bin/sh
d="$HOME/.claude/fake"
mkdir -p "$d"
n=$(( $(ls "$d" | grep -c '^args-') + 1 ))
export FAKE_CLAUDE_ARGS_FILE="$d/args-$n"
export FAKE_CLAUDE_STDIN_FILE="$d/stdin-$n"
printf '%s' "${{ANTHROPIC_API_KEY-}}" > "$d/key-$n"
printf '%s' "${{CLAUDECODE-unset}}" > "$d/claudecode-$n"
pwd > "$d/cwd-$n"
{pick}
export FAKE_CLAUDE_SCENARIO="{scenarios}/$sc.ndjson"
exec "{fake}" "$@"
"#,
        scenarios = scenarios.display(),
        fake = fake.display(),
    );
    let path = dir.join("bin/claude");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn cc_profile(command: &str, api_key: ApiKeySource) -> Profile {
    Profile {
        id: "cc".into(),
        name: "Claude Code".into(),
        kind: BackendKind::ClaudeCode,
        base_url: String::new(),
        api_key,
        model: String::new(),
        context_limit: 1000,
        max_output: 100,
        extra_body: Default::default(),
        command: Some(command.into()),
    }
}

/// Start the engine on a workspace (`workspace`) or Scratch. None when the sandbox isn't here.
fn rig_with(setup: Setup, workspace: bool) -> Option<Rig> {
    let tmp = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let home = tmp.path().join("home");
    let ws = home.join("proj");
    let data = home.join(".local/share/void.ai");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::write(home.join(".claude.json"), "{}").unwrap();
    // A package cache, so spawns take the sandbox's cache lock.
    fs::create_dir_all(home.join(".cache/pip")).unwrap();
    fs::write(ws.join("a.txt"), "orig\n").unwrap();
    let command = setup
        .command
        .clone()
        .unwrap_or_else(|| wrapper(tmp.path(), &setup.pick).display().to_string());
    let mut sandbox = SandboxSettings {
        caches: vec!["~/.cache/pip".into()],
        network: setup.network,
        ..SandboxSettings::default()
    };
    // The real Claude Code state stays hidden even though HOME is a dummy.
    if let Some(real) = std::env::var_os("HOME") {
        for p in [".claude", ".claude.json", ".config/Claude"] {
            sandbox
                .hidden
                .push(Path::new(&real).join(p).to_string_lossy().into_owned());
        }
    }
    let cfg = Config {
        active_profile: "cc".into(),
        profiles: vec![cc_profile(&command, setup.api_key.clone())],
        sandbox,
        ..Config::default()
    };
    let cfg_path = tmp.path().join("config/config.toml");
    save_config(&cfg_path, &cfg).unwrap();
    let store = Store::new(data);
    let wsm = store.workspace_for(&ws).unwrap();
    for mut c in setup.before {
        c.workspace_id = if workspace {
            wsm.id.clone()
        } else {
            Workspace::scratch().id
        };
        store.save(&c).unwrap();
    }
    let handle = Engine::start_with_options(
        cfg_path,
        store.clone(),
        Arc::new(|| {}),
        Arc::new(void_core::backend::make_backend),
        EngineOptions {
            home: Some(home.clone()),
            ..EngineOptions::default()
        },
    );
    let mut r = Rig {
        tmp,
        home,
        ws: ws.clone(),
        store,
        handle,
    };
    match r.until(|e| matches!(e, Event::SandboxStatus(_))) {
        Event::SandboxStatus(Availability::Ready { .. }) => {}
        other => {
            eprintln!("skipping: sandbox unavailable: {other:?}");
            return None;
        }
    }
    if workspace {
        r.handle.send(Command::OpenWorkspace(Some(ws)));
        r.until(
            |e| matches!(e, Event::WorkspaceOpened { workspace, .. } if workspace.root.is_some()),
        );
    }
    Some(r)
}

fn rig(setup: Setup) -> Option<Rig> {
    rig_with(setup, true)
}

impl Drop for Rig {
    /// overlayfs leaves a mode-000 `work/work` behind, which stops TempDir's cleanup.
    fn drop(&mut self) {
        fn open_up(p: &Path) {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(p, fs::Permissions::from_mode(0o700));
            if let Ok(rd) = fs::read_dir(p) {
                for e in rd.flatten() {
                    if e.file_type().is_ok_and(|t| t.is_dir()) {
                        open_up(&e.path());
                    }
                }
            }
        }
        open_up(self.tmp.path());
    }
}

impl Rig {
    fn next(&mut self) -> Event {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(e) = self.handle.try_recv() {
                return e;
            }
            assert!(Instant::now() < deadline, "timed out waiting for an event");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn until(&mut self, mut pred: impl FnMut(&Event) -> bool) -> Event {
        loop {
            let e = self.next();
            if pred(&e) {
                return e;
            }
        }
    }

    fn new_conversation(&mut self) -> ConversationId {
        self.handle.send(Command::NewConversation);
        match self.until(|e| matches!(e, Event::ConversationCreated(_))) {
            Event::ConversationCreated(c) => c.id,
            _ => unreachable!(),
        }
    }

    fn send(&self, conversation: ConversationId, text: &str) {
        self.handle.send(Command::SendMessage {
            conversation,
            text: text.into(),
        });
    }

    fn turn_over(&mut self, c: ConversationId) {
        self.until(
            |e| matches!(e, Event::TurnState { conversation, active: false } if *conversation == c),
        );
    }

    fn saved(&self, c: ConversationId, workspace: bool) -> Conversation {
        let ws = if workspace {
            self.store.workspace_for(&self.ws).unwrap().id
        } else {
            Workspace::scratch().id
        };
        self.store.load(&ws, &c).unwrap()
    }

    fn overlay(&self, c: ConversationId) -> Overlay {
        let w = self.store.workspace_for(&self.ws).unwrap();
        Overlay::open(&self.store.overlay_dir(&w.id, &c), &self.ws).unwrap()
    }

    fn fake(&self, name: &str) -> String {
        fs::read_to_string(self.home.join(".claude/fake").join(name)).unwrap_or_default()
    }

    fn args(&self, n: usize) -> Vec<String> {
        self.fake(&format!("args-{n}"))
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// The user messages a spawn received on stdin.
    fn user_inputs(&self, n: usize) -> Vec<String> {
        self.fake(&format!("stdin-{n}"))
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["type"] == "user")
            .map(|v| {
                v["message"]["content"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    }
}

fn earlier_chat() -> Conversation {
    let mut c = Conversation::new(Workspace::scratch().id);
    c.messages.push(Message::user("earlier question"));
    c.messages.push(Message::new(
        Role::Assistant,
        vec![Part::Text("earlier answer".into())],
    ));
    c
}

#[test]
fn claude_turn_streams_and_stores_parts() {
    let Some(mut r) = rig(Setup {
        pick: r#"if [ "$n" = 1 ]; then sc=tool-use; else sc=text-turn; fi"#.into(),
        ..Setup::default()
    }) else {
        return;
    };
    let c = r.new_conversation();
    r.send(c, "list them");
    r.until(|e| matches!(e, Event::StreamStarted(id) if *id == c));
    match r.until(|e| matches!(e, Event::ClaudeInit { .. })) {
        Event::ClaudeInit {
            conversation,
            model,
        } => {
            assert_eq!(conversation, c);
            assert_eq!(model, "claude-fake");
        }
        _ => unreachable!(),
    }
    match r.until(|e| matches!(e, Event::ToolStarted { .. })) {
        Event::ToolStarted {
            call_id,
            name,
            summary,
            ..
        } => {
            assert_eq!((call_id.as_str(), name.as_str()), ("tu_1", "Bash"));
            assert_eq!(summary, "ls");
        }
        _ => unreachable!(),
    }
    match r.until(|e| matches!(e, Event::ToolFinished { .. })) {
        Event::ToolFinished {
            call_id, output, ..
        } => {
            assert_eq!(call_id, "tu_1");
            assert_eq!(output.content, "a.txt\nb.txt");
            assert!(!output.is_error);
        }
        _ => unreachable!(),
    }
    r.until(
        |e| matches!(e, Event::StreamFinished { message, .. } if message.text() == "Two files."),
    );
    r.turn_over(c);
    let saved = r.saved(c, true);
    let roles: Vec<Role> = saved.messages.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        [Role::User, Role::Assistant, Role::Tool, Role::Assistant]
    );
    assert_eq!(
        saved.messages[1].parts,
        [
            Part::Reasoning("list files".into()),
            Part::ToolCall {
                id: "tu_1".into(),
                name: "Bash".into(),
                arguments: r#"{"command":"ls"}"#.into()
            }
        ]
    );
    assert_eq!(
        saved.messages[2].parts,
        [Part::ToolResult {
            call_id: "tu_1".into(),
            content: "a.txt\nb.txt".into()
        }]
    );
    let last = &saved.messages[3];
    assert_eq!(last.text(), "Two files.");
    assert_eq!(last.usage.map(|u| u.prompt_tokens), Some(20));
    assert_eq!(saved.claude_session, Some(c.0));

    let args = r.args(1);
    assert!(args.contains(&format!("--session-id={}", c.0)), "{args:?}");
    let ap = args
        .iter()
        .position(|a| a == "--append-system-prompt")
        .unwrap();
    assert_eq!(
        args[ap + 1],
        "Your file changes are staged for the user's review in void.ai, not written to disk; \
         the user accepts or rejects them."
    );
    assert!(!args.iter().any(|a| a == "--system-prompt"), "{args:?}");
    assert!(!args.iter().any(|a| a == "--permission-mode"), "{args:?}");
    assert!(
        args.windows(2)
            .any(|w| w == ["--permission-prompts", "host"])
    );
    assert_eq!(r.user_inputs(1), ["list them"]);
    assert_eq!(r.fake("cwd-1").trim(), r.ws.display().to_string());
    assert_eq!(r.fake("key-1"), "", "no API key without a key source");
    assert_eq!(r.fake("claudecode-1"), "unset");
    let stdin = r.fake("stdin-1");
    let first: Value = serde_json::from_str(stdin.lines().next().unwrap()).unwrap();
    assert_eq!(first["request"]["subtype"], "initialize", "{stdin}");

    // Deltas stream as they come, and the final assistant frame doesn't double them.
    r.send(c, "hi");
    let mut deltas = String::new();
    let msg = loop {
        match r.next() {
            Event::Delta {
                kind: DeltaKind::Text,
                text,
                ..
            } => deltas.push_str(&text),
            Event::StreamFinished { message, .. } => break message,
            _ => {}
        }
    };
    assert_eq!(deltas, "Hello world");
    assert_eq!(msg.text(), "Hello world");
    assert_eq!(msg.usage.map(|u| u.prompt_tokens), Some(10));
    r.turn_over(c);
}

#[test]
fn claude_turn_edits_land_in_overlay() {
    let Some(mut r) = rig(Setup {
        pick: "sc=write-file".into(),
        ..Setup::default()
    }) else {
        return;
    };
    let c = r.new_conversation();
    r.send(c, "write it");
    r.until(
        |e| matches!(e, Event::StreamFinished { message, .. } if message.text() == "Wrote a.txt."),
    );
    match r.until(|e| matches!(e, Event::Changes { conversation, .. } if *conversation == c)) {
        Event::Changes { changes, .. } => {
            assert!(
                changes.iter().any(|ch| ch.path == Path::new("a.txt")),
                "{changes:?}"
            )
        }
        _ => unreachable!(),
    }
    assert_eq!(fs::read_to_string(r.ws.join("a.txt")).unwrap(), "orig\n");
    assert_eq!(
        r.overlay(c).read(Path::new("a.txt")).unwrap(),
        b"from claude\n"
    );
    // The context meter follows the last API call's usage, not the turn's cumulative total.
    let last = r.saved(c, true).messages.last().unwrap().clone();
    assert_eq!(last.usage.map(|u| u.prompt_tokens), Some(130));
}

#[test]
fn first_claude_turn_prepends_transcript_once() {
    let before = earlier_chat();
    let c = before.id;
    let Some(mut r) = rig(Setup {
        before: vec![before],
        ..Setup::default()
    }) else {
        return;
    };
    r.send(c, "new question");
    r.turn_over(c);
    r.send(c, "follow-up");
    r.turn_over(c);
    let first = r.user_inputs(1);
    assert_eq!(first.len(), 1);
    let t = &first[0];
    assert!(t.starts_with("Earlier in this chat:"), "{t}");
    assert!(
        t.contains("earlier question") && t.contains("earlier answer"),
        "{t}"
    );
    assert!(t.ends_with("new question"), "{t}");
    assert!(r.args(1).contains(&format!("--session-id={}", c.0)));
    assert_eq!(r.user_inputs(2), ["follow-up"]);
    assert!(r.args(2).contains(&format!("--resume={}", c.0)));
    // The stored history is untouched by the prepend.
    let saved = r.saved(c, true);
    let users: Vec<String> = saved
        .messages
        .iter()
        .filter(|m| m.role == Role::User)
        .map(Message::text)
        .collect();
    assert_eq!(users, ["earlier question", "new question", "follow-up"]);
}

#[test]
fn resume_uses_session() {
    let mut before = Conversation::new(Workspace::scratch().id);
    let session = Uuid::now_v7();
    before.claude_session = Some(session);
    let c = before.id;
    let Some(mut r) = rig(Setup {
        before: vec![before],
        ..Setup::default()
    }) else {
        return;
    };
    r.send(c, "again");
    r.turn_over(c);
    let args = r.args(1);
    assert!(args.contains(&format!("--resume={session}")), "{args:?}");
    assert!(
        !args.iter().any(|a| a.starts_with("--session-id")),
        "{args:?}"
    );
    assert_eq!(r.user_inputs(1), ["again"]);
    assert_eq!(r.saved(c, true).claude_session, Some(session));
}

#[test]
fn missing_session_restarts_with_transcript() {
    let mut before = earlier_chat();
    let gone = Uuid::now_v7();
    before.claude_session = Some(gone);
    let c = before.id;
    let Some(mut r) = rig(Setup {
        pick: r#"case "$*" in *--resume=*) sc=missing-session;; *) sc=text-turn;; esac"#.into(),
        before: vec![before],
        ..Setup::default()
    }) else {
        return;
    };
    r.send(c, "still there?");
    let msg =
        match r.until(|e| matches!(e, Event::StreamFinished { .. } | Event::StreamFailed { .. })) {
            Event::StreamFinished { message, .. } => message,
            e => panic!("{e:?}"),
        };
    assert_eq!(msg.text(), "Hello world");
    r.turn_over(c);
    assert!(r.args(1).contains(&format!("--resume={gone}")));
    let fresh = r
        .args(2)
        .iter()
        .find_map(|a| a.strip_prefix("--session-id=").map(str::to_string))
        .expect("restarted with a new session");
    let fresh = Uuid::parse_str(&fresh).unwrap();
    assert_ne!(fresh, gone);
    let t = &r.user_inputs(2)[0];
    assert!(t.starts_with("Earlier in this chat:"), "{t}");
    assert!(
        t.contains("earlier answer") && t.ends_with("still there?"),
        "{t}"
    );
    let saved = r.saved(c, true);
    assert_eq!(saved.claude_session, Some(fresh));
    assert!(saved.messages.iter().all(|m| m.error.is_none()));
}

#[test]
fn stop_kills_and_keeps_partial() {
    let Some(mut r) = rig(Setup {
        pick: r#"if [ "$n" = 1 ]; then sc=slow; else sc=text-turn; fi"#.into(),
        ..Setup::default()
    }) else {
        return;
    };
    let c = r.new_conversation();
    r.send(c, "go");
    r.until(|e| matches!(e, Event::Delta { text, .. } if text == "answer"));
    let t = Instant::now();
    r.handle.send(Command::Stop(c));
    let msg = match r.until(|e| matches!(e, Event::StreamFinished { .. })) {
        Event::StreamFinished { message, .. } => message,
        _ => unreachable!(),
    };
    assert!(msg.stopped);
    assert_eq!(msg.text(), "partial answer");
    r.turn_over(c);
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    let saved = r.saved(c, true);
    assert!(saved.messages.last().unwrap().stopped);
    // The chat is free again.
    r.send(c, "next");
    r.until(
        |e| matches!(e, Event::StreamFinished { message, .. } if message.text() == "Hello world"),
    );
}

#[test]
fn crash_without_result_fails_turn() {
    let Some(mut r) = rig(Setup {
        pick: "sc=crash".into(),
        api_key: ApiKeySource::Literal("sk-secret-do-not-show".into()),
        ..Setup::default()
    }) else {
        return;
    };
    let c = r.new_conversation();
    r.send(c, "go");
    let error = match r.until(|e| matches!(e, Event::StreamFailed { .. })) {
        Event::StreamFailed { error, .. } => error,
        _ => unreachable!(),
    };
    assert!(error.contains("fatal: the fake crashed"), "{error}");
    assert!(error.contains("something went badly wrong"), "{error}");
    assert!(!error.contains("sk-secret"), "{error}");
    r.turn_over(c);
    let saved = r.saved(c, true);
    assert_eq!(
        saved.messages.last().unwrap().error.as_deref(),
        Some(&*error)
    );
}

#[test]
fn auth_error_message() {
    let Some(mut r) = rig(Setup {
        pick: "sc=auth-error".into(),
        ..Setup::default()
    }) else {
        return;
    };
    let c = r.new_conversation();
    r.send(c, "hello");
    match r.until(|e| matches!(e, Event::StreamFailed { .. })) {
        Event::StreamFailed { error, .. } => assert_eq!(error, NOT_SIGNED_IN),
        _ => unreachable!(),
    }
    r.turn_over(c);
}

#[test]
fn review_refused_during_claude_turn() {
    let Some(mut r) = rig(Setup {
        pick: "sc=slow".into(),
        ..Setup::default()
    }) else {
        return;
    };
    let other = r.new_conversation();
    r.overlay(other)
        .write(Path::new("a.txt"), b"other chat\n")
        .unwrap();
    let c = r.new_conversation();
    r.send(c, "go");
    r.until(|e| matches!(e, Event::Delta { .. }));
    r.handle.send(Command::AcceptAll {
        conversation: other,
        keep_ignored: false,
    });
    match r.until(|e| matches!(e, Event::Error(_))) {
        Event::Error(m) => assert_eq!(m, BUSY),
        _ => unreachable!(),
    }
    r.handle.send(Command::RejectFile {
        conversation: other,
        path: "a.txt".into(),
    });
    match r.until(|e| matches!(e, Event::Error(_))) {
        Event::Error(m) => assert_eq!(m, BUSY),
        _ => unreachable!(),
    }
    assert_eq!(fs::read_to_string(r.ws.join("a.txt")).unwrap(), "orig\n");
    assert_eq!(
        r.overlay(other).read(Path::new("a.txt")).unwrap(),
        b"other chat\n"
    );
    r.handle.send(Command::SaveToOverlay {
        request: 9,
        conversation: c,
        path: "a.txt".into(),
        content: "x\n".into(),
    });
    match r.until(|e| matches!(e, Event::OverlaySaveFailed { .. })) {
        Event::OverlaySaveFailed { request, error, .. } => {
            assert_eq!(request, 9);
            assert_eq!(error, BUSY);
        }
        _ => unreachable!(),
    }
    // A second message to the running chat is refused, not a second `claude`.
    r.send(c, "again");
    r.until(|e| matches!(e, Event::Error(m) if m.contains("still working")));
    r.handle.send(Command::Stop(c));
    r.turn_over(c);
    assert_eq!(r.fake("args-2"), "", "only one claude ran");
    // Once it's over, review works again.
    r.handle.send(Command::AcceptAll {
        conversation: other,
        keep_ignored: false,
    });
    r.until(|e| matches!(e, Event::Changes { conversation, changes } if *conversation == other && changes.is_empty()));
    assert_eq!(
        fs::read_to_string(r.ws.join("a.txt")).unwrap(),
        "other chat\n"
    );
}

#[test]
fn second_turn_waits_for_lock() {
    let Some(mut r) = rig(Setup {
        pick: r#"if [ "$n" = 1 ]; then sc=brief-wait; else sc=text-turn; fi"#.into(),
        ..Setup::default()
    }) else {
        return;
    };
    let a = r.new_conversation();
    let b = r.new_conversation();
    r.send(a, "first");
    r.until(|e| matches!(e, Event::ClaudeInit { conversation, .. } if *conversation == a));
    r.send(b, "second");
    match r.until(|e| matches!(e, Event::TurnStatus { conversation, .. } if *conversation == b)) {
        Event::TurnStatus { status, .. } => {
            assert!(status.to_lowercase().contains("waiting"), "{status}")
        }
        _ => unreachable!(),
    }
    let mut order = Vec::new();
    while order.len() < 2 {
        if let Event::StreamFinished { conversation, .. } = r.next() {
            order.push(conversation);
        }
    }
    assert_eq!(order, [a, b]);
    assert_eq!(r.user_inputs(1), ["first"]);
    assert_eq!(r.user_inputs(2), ["second"]);
}

#[test]
fn scratch_claude_turn_runs_in_scratch_dir() {
    let Some(mut r) = rig_with(Setup::default(), false) else {
        return;
    };
    let c = r.new_conversation();
    r.send(c, "hi");
    r.until(
        |e| matches!(e, Event::StreamFinished { message, .. } if message.text() == "Hello world"),
    );
    r.turn_over(c);
    let dir = r
        .store
        .data_dir()
        .join("claude-scratch")
        .join(c.to_string());
    assert!(dir.is_dir());
    assert_eq!(r.fake("cwd-1").trim(), dir.display().to_string());
    assert_eq!(r.saved(c, false).messages.len(), 2);
}

#[test]
fn retry_claude_turn() {
    let Some(mut r) = rig(Setup {
        pick: r#"if [ "$n" = 1 ]; then sc=write-file; else sc=text-turn; fi"#.into(),
        ..Setup::default()
    }) else {
        return;
    };
    let c = r.new_conversation();
    r.send(c, "write it");
    r.turn_over(c);
    assert_eq!(
        r.overlay(c).read(Path::new("a.txt")).unwrap(),
        b"from claude\n"
    );
    r.handle.send(Command::Retry(c));
    r.until(
        |e| matches!(e, Event::StreamFinished { message, .. } if message.text() == "Hello world"),
    );
    r.turn_over(c);
    assert!(
        r.overlay(c).changes().is_empty(),
        "the retried attempt's edit is undone"
    );
    assert!(r.args(2).contains(&format!("--resume={}", c.0)));
    let t = &r.user_inputs(2)[0];
    assert_eq!(t, &format!("{RETRY_NOTE}\n\nwrite it"));
    let saved = r.saved(c, true);
    let roles: Vec<Role> = saved.messages.iter().map(|m| m.role).collect();
    assert_eq!(roles, [Role::User, Role::Assistant]);
    assert_eq!(saved.messages[0].text(), "write it");
}

#[test]
fn api_key_env_passed() {
    let Some(mut r) = rig(Setup {
        api_key: ApiKeySource::Literal("sk-test-key".into()),
        ..Setup::default()
    }) else {
        return;
    };
    let c = r.new_conversation();
    r.handle
        .send(Command::SetConversationAgent(c, Some("plan".into())));
    r.send(c, "hi");
    r.turn_over(c);
    assert_eq!(r.fake("key-1"), "sk-test-key");
    let args = r.args(1);
    assert!(
        args.windows(2).any(|w| w == ["--permission-mode", "plan"]),
        "{args:?}"
    );
}

#[test]
fn missing_binary_fails_turn() {
    let Some(mut r) = rig(Setup {
        command: Some("no-such-claude-binary".into()),
        ..Setup::default()
    }) else {
        return;
    };
    let c = r.new_conversation();
    r.send(c, "hi");
    match r.until(|e| matches!(e, Event::StreamFailed { .. })) {
        Event::StreamFailed { error, .. } => {
            assert!(error.contains("no-such-claude-binary"), "{error}");
            assert!(error.contains("not found"), "{error}");
            assert!(error.contains("install Claude Code"), "{error}");
        }
        _ => unreachable!(),
    }
    r.turn_over(c);
}

#[test]
fn network_off_fails_turn() {
    let Some(mut r) = rig(Setup {
        network: false,
        ..Setup::default()
    }) else {
        return;
    };
    let c = r.new_conversation();
    r.send(c, "hi");
    match r.until(|e| matches!(e, Event::StreamFailed { .. })) {
        Event::StreamFailed { error, .. } => assert!(error.contains("network"), "{error}"),
        _ => unreachable!(),
    }
    r.turn_over(c);
    assert_eq!(r.fake("args-1"), "", "claude never started");
}

#[test]
fn control_requests_are_answered() {
    let Some(mut r) = rig(Setup {
        pick: "sc=control".into(),
        ..Setup::default()
    }) else {
        return;
    };
    let c = r.new_conversation();
    r.send(c, "go");
    match r.until(|e| matches!(e, Event::TurnStatus { .. })) {
        Event::TurnStatus { status, .. } => assert!(status.contains("retrying"), "{status}"),
        _ => unreachable!(),
    }
    r.until(
        |e| matches!(e, Event::StreamFinished { message, .. } if message.text() == "Answered."),
    );
    r.turn_over(c);
    let answers: Vec<Value> = r
        .fake("stdin-1")
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["type"] == "control_response")
        .collect();
    assert_eq!(answers.len(), 2, "{answers:?}");
    let hook = &answers[0]["response"];
    assert_eq!(hook["request_id"], "hook-1");
    assert_eq!(hook["subtype"], "error");
    assert_eq!(
        hook["error"],
        "Unsupported control request subtype: hook_callback"
    );
    let perm = &answers[1]["response"];
    assert_eq!(perm["request_id"], "perm-1");
    assert_eq!(perm["response"]["behavior"], "deny");
}
