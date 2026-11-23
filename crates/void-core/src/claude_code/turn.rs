//! One chat turn through Claude Code: the `claude` process in the sandbox, its stdin and stdout,
//! and the mapping of its stream onto void's parts and events. The engine owns the state; this
//! module runs the process ([`run_turn`]) and turns its events into engine actions
//! ([`Segments`]).

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::ChildStdin;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    CcEvent, SessionMode, TurnArgs, build_args, control_response_error, initialize_request,
    parse_events, user_message,
};
use crate::config::{ApiKeySource, resolve_api_key};
use crate::context::estimate_tokens;
use crate::engine::DeltaKind;
use crate::model::{Conversation, ConversationId, Message, Part, Role, Usage};
use crate::sandbox::{OverlayMounts, RwBind, Sandbox, SpawnPlan};

/// `--append-system-prompt`: the only thing void adds to Claude Code's own prompt.
pub const VOID_NOTE: &str = "Your file changes are staged for the user's review in void.ai, not \
                             written to disk; the user accepts or rejects them.";
/// Prepended to a retried user message (the CLI can't resume from before the retried turn).
pub const RETRY_NOTE: &str = "The previous attempt was undone; its file changes were reverted.";
pub const NOT_SIGNED_IN: &str =
    "Claude Code isn't signed in — run `claude auth login` in a terminal";

/// The not-signed-in error: void's hint first (Claude Code's own advice can be for its terminal
/// UI, e.g. "/login"), then what Claude Code said, if anything (already redacted), capped.
pub fn not_signed_in(said: &str) -> String {
    match said.trim() {
        "" => NOT_SIGNED_IN.to_string(),
        said => {
            let said: String = said.chars().take(STDERR_LINE_CAP).collect();
            format!("{NOT_SIGNED_IN} (Claude Code said: {})", said.trim_end())
        }
    }
}
/// Review actions and chat-view saves while a turn has the overlay mounted.
pub const BUSY: &str = "A Claude Code turn is running — try again when it finishes";

/// The earlier transcript sent with a chat's first Claude Code turn, at most this many bytes.
const TRANSCRIPT_CAP: usize = 200 * 1024;
/// Each tool result in that transcript, at most this many bytes.
const TRANSCRIPT_TOOL_CAP: usize = 4 * 1024;
const STDERR_LINES: usize = 20;
const STDERR_LINE_CAP: usize = 500;
/// After the result, how long `claude` gets to exit once stdin is closed.
const EXIT_GRACE: Duration = Duration::from_secs(10);

/// Lines for `claude`'s stdin (without the newline); permission answers go here too.
pub type StdinTx = mpsc::UnboundedSender<String>;

/// A `can_use_tool` request from Claude Code.
#[derive(Clone, Debug, PartialEq)]
pub struct PermissionAsk {
    pub request_id: String,
    pub tool: String,
    pub input: Value,
    pub suggestions: Option<Value>,
    /// Claude Code's own wording and the path that made it ask, when it sends them.
    pub title: Option<String>,
    pub description: Option<String>,
    pub blocked_path: Option<String>,
}

/// Where Claude Code's permission requests go. Each request must eventually get exactly one
/// `control_response_*` line on `stdin` (Claude Code waits for it), unless it is cancelled.
pub trait PermissionBridge: Send + Sync {
    fn ask(&self, conversation: ConversationId, ask: PermissionAsk, stdin: StdinTx);
    /// Claude Code withdrew a request (`control_cancel_request`): drop it, answer nothing.
    fn cancel(&self, conversation: ConversationId, request_id: &str);
    /// The chat's turn was stopped, deleted or is over: drop anything still pending for it.
    /// May be called more than once per turn.
    fn turn_ended(&self, _conversation: ConversationId) {}
}

/// Find the profile's `claude`: a path (a leading `~/` is the home directory) or a bare name,
/// looked up on `PATH` and then in `~/.local/bin` (desktop launches often lack it on `PATH`).
pub fn resolve_command(command: &str, home: &Path) -> Result<PathBuf, String> {
    let command = command.trim();
    let missing = || {
        format!(
            "`{command}` not found — install Claude Code or set the full path in the profile's \
             Command"
        )
    };
    let path = match command.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(command),
    };
    if command.is_empty() {
        return Err(missing());
    }
    if command.contains('/') {
        return Some(path).filter(|p| executable(p)).ok_or_else(missing);
    }
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .chain([home.join(".local/bin")])
        .map(|d| d.join(command))
        .find(|p| executable(p))
        .ok_or_else(missing)
}

fn executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The chat so far, as plain text for a fresh Claude Code session: the latest compaction
/// summary and everything after it, before message `upto`. Oldest text is cut first.
pub fn transcript(c: &Conversation, upto: usize) -> String {
    let msgs = &c.messages[..upto.min(c.messages.len())];
    let from = msgs
        .iter()
        .rposition(|m| m.parts.iter().any(|p| matches!(p, Part::Summary(_))))
        .unwrap_or(0);
    let mut blocks = Vec::new();
    for m in &msgs[from..] {
        let who = match m.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::Tool => "Tool",
            Role::System => continue,
        };
        for p in &m.parts {
            match p {
                Part::Text(t) if !t.trim().is_empty() => blocks.push(format!("{who}: {t}")),
                Part::Summary(s) => {
                    blocks.push(format!("Summary of the earlier conversation:\n{s}"))
                }
                Part::ToolCall {
                    name, arguments, ..
                } => blocks.push(format!("{who} used the tool {name}: {arguments}")),
                Part::ToolResult { content, .. } => blocks.push(format!(
                    "Tool result:\n{}",
                    tail(content, TRANSCRIPT_TOOL_CAP)
                )),
                _ => {}
            }
        }
    }
    let text = blocks.join("\n\n");
    if text.len() <= TRANSCRIPT_CAP {
        return text;
    }
    format!(
        "[… earlier messages omitted]\n{}",
        tail(&text, TRANSCRIPT_CAP)
    )
}

/// The last `max` bytes of `s`, on a char boundary.
fn tail(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// `message` with the earlier transcript (if there is one) in front of it.
pub fn with_transcript(c: &Conversation, upto: usize, message: &str) -> String {
    let t = transcript(c, upto);
    if t.is_empty() {
        message.to_string()
    } else {
        format!("Earlier in this chat:\n\n{t}\n\n---\n\n{message}")
    }
}

/// A short label for a Claude Code tool call (its command, file, pattern, …).
pub fn tool_summary(arguments: &str) -> String {
    let v: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let keys = [
        "command",
        "file_path",
        "path",
        "pattern",
        "url",
        "description",
        "prompt",
    ];
    keys.iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .map(|s| s.lines().next().unwrap_or("").chars().take(120).collect())
        .unwrap_or_default()
}

/// The result recorded for a tool call whose turn ended (Stop, a failure) before it finished.
pub const STOPPED_TOOL: &str = "(stopped before it finished)";

/// What the engine does with a mapped event.
#[derive(Clone, Debug, PartialEq)]
pub enum Out {
    StreamStarted,
    Delta(DeltaKind, String),
    /// Append this assistant message; StreamFinished, or StreamFailed when it has an error.
    Finished {
        message: Message,
        tokens_per_sec: Option<f32>,
    },
    ToolStarted {
        call_id: String,
        name: String,
        summary: String,
    },
    ToolOutput {
        call_id: String,
        text: String,
    },
    /// Append a tool message with this result; ToolFinished.
    ToolFinished {
        call_id: String,
        content: String,
        is_error: bool,
    },
}

/// Splits a Claude Code turn into void's shape: assistant messages (one per stretch between
/// tool results) and tool messages, streamed as deltas and finished like a void reply.
#[derive(Debug, Default)]
pub struct Segments {
    open: bool,
    /// Parts from complete `assistant` frames.
    committed: Vec<Part>,
    /// Text and thinking from deltas not yet confirmed by an `assistant` frame.
    partial: Vec<Part>,
    /// The latest main-thread API call's usage in this segment / in the whole turn.
    usage: Option<Usage>,
    turn_usage: Option<Usage>,
    first_delta: Option<Instant>,
    /// Main-thread tool calls started (ToolStarted) whose results haven't come yet.
    open_calls: Vec<String>,
}

impl Segments {
    fn open(&mut self, out: &mut Vec<Out>) {
        if !self.open {
            self.open = true;
            self.first_delta = None;
            out.push(Out::StreamStarted);
        }
    }

    /// The turn begins: show it as streaming.
    pub fn start(&mut self) -> Vec<Out> {
        let mut out = Vec::new();
        self.open(&mut out);
        out
    }

    /// Forget the open segment's content (a failed attempt before a restart). Returns the
    /// tool calls it had started that never finished: they're in no stored message, so the
    /// engine closes their cards without recording a result.
    pub fn reset(&mut self) -> Vec<String> {
        self.committed.clear();
        self.partial.clear();
        self.usage = None;
        self.turn_usage = None;
        std::mem::take(&mut self.open_calls)
    }

    pub fn on_event(&mut self, ev: CcEvent) -> Vec<Out> {
        let mut out = Vec::new();
        match ev {
            CcEvent::TextDelta { text, parent: None } => {
                self.delta(&mut out, text, DeltaKind::Text);
            }
            CcEvent::ThinkingDelta { text, parent: None } => {
                self.delta(&mut out, text, DeltaKind::Reasoning);
            }
            // A subagent's text shows on its parent's tool card; its result is the parent's.
            CcEvent::TextDelta {
                text,
                parent: Some(p),
            } => out.push(Out::ToolOutput { call_id: p, text }),
            CcEvent::Assistant {
                parts,
                parent: None,
                usage,
                ..
            } => {
                self.open(&mut out);
                self.partial.clear();
                for p in &parts {
                    if let Part::ToolCall {
                        id,
                        name,
                        arguments,
                    } = p
                    {
                        self.open_calls.push(id.clone());
                        out.push(Out::ToolStarted {
                            call_id: id.clone(),
                            name: name.clone(),
                            summary: tool_summary(arguments),
                        });
                    }
                }
                self.committed.extend(parts);
                if usage.is_some() {
                    self.usage = usage;
                    self.turn_usage = usage;
                }
            }
            CcEvent::Assistant {
                parts,
                parent: Some(p),
                ..
            } => {
                for part in parts {
                    if let Part::ToolCall {
                        name, arguments, ..
                    } = part
                    {
                        out.push(Out::ToolOutput {
                            call_id: p.clone(),
                            text: format!("› {name} {}\n", tool_summary(&arguments)),
                        });
                    }
                }
            }
            CcEvent::ToolResult {
                call_id,
                content,
                is_error,
                parent: None,
            } => {
                self.open_calls.retain(|c| *c != call_id);
                if self.open && !(self.committed.is_empty() && self.partial.is_empty()) {
                    out.extend(self.finish(false, None, None));
                }
                out.push(Out::ToolFinished {
                    call_id,
                    content,
                    is_error,
                });
            }
            _ => {}
        }
        out
    }

    fn delta(&mut self, out: &mut Vec<Out>, text: String, kind: DeltaKind) {
        self.open(out);
        self.first_delta.get_or_insert_with(Instant::now);
        let reasoning = kind == DeltaKind::Reasoning;
        match (self.partial.last_mut(), reasoning) {
            (Some(Part::Text(s)), false) | (Some(Part::Reasoning(s)), true) => s.push_str(&text),
            _ => self.partial.push(if reasoning {
                Part::Reasoning(text.clone())
            } else {
                Part::Text(text.clone())
            }),
        }
        out.push(Out::Delta(kind, text));
    }

    /// End the turn: [`finish`](Self::finish) the open segment, then finish every tool call
    /// still running as stopped ([`STOPPED_TOOL`], an error), after the message holding it.
    pub fn end(
        &mut self,
        stopped: bool,
        error: Option<String>,
        result_usage: Option<Usage>,
    ) -> Vec<Out> {
        let mut out: Vec<Out> = self
            .finish(stopped, error, result_usage)
            .into_iter()
            .collect();
        out.extend(
            std::mem::take(&mut self.open_calls)
                .into_iter()
                .map(|call_id| Out::ToolFinished {
                    call_id,
                    content: STOPPED_TOOL.into(),
                    is_error: true,
                }),
        );
        out
    }

    /// End the open segment as a message (`stopped` keeps the partial text; `error` makes it a
    /// failure). `result_usage` (the turn's cumulative usage) is only used when no API call
    /// reported its own. Nothing when no segment is open, unless there's an error to report.
    pub fn finish(
        &mut self,
        stopped: bool,
        error: Option<String>,
        result_usage: Option<Usage>,
    ) -> Option<Out> {
        if !self.open && error.is_none() {
            return None;
        }
        self.open = false;
        let mut parts = std::mem::take(&mut self.committed);
        parts.append(&mut self.partial);
        let mut message = Message::new(Role::Assistant, parts);
        let usage = self.usage.take();
        message.usage = match (usage, self.turn_usage) {
            (Some(u), _) => Some(u),
            (None, None) => result_usage.filter(|u| *u != Usage::default()),
            (None, Some(_)) => None,
        };
        message.stopped = stopped;
        message.error = error;
        let tokens_per_sec = self.first_delta.take().and_then(|t| {
            let secs = t.elapsed().as_secs_f32();
            let tokens = message
                .usage
                .map(|u| u.completion_tokens)
                .unwrap_or_else(|| {
                    estimate_tokens(&message.text()) + estimate_tokens(&message.reasoning())
                });
            (secs > 0.001 && tokens > 0).then(|| tokens as f32 / secs)
        });
        Some(Out::Finished {
            message,
            tokens_per_sec,
        })
    }
}

/// Reports for a turn's task, and the turn's end if the task dies without reporting it.
pub struct Backstop<F: Fn(TurnUpdate)> {
    send: F,
    exited: std::sync::atomic::AtomicBool,
}

impl<F: Fn(TurnUpdate)> Backstop<F> {
    pub fn new(send: F) -> Self {
        Self {
            send,
            exited: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn send(&self, update: TurnUpdate) {
        if matches!(update, TurnUpdate::Exited { .. }) {
            self.exited.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        (self.send)(update)
    }
}

impl<F: Fn(TurnUpdate)> Drop for Backstop<F> {
    fn drop(&mut self) {
        if !self.exited.load(std::sync::atomic::Ordering::SeqCst) {
            (self.send)(TurnUpdate::Exited {
                error: Some("Claude Code's turn ended unexpectedly (a void.ai bug).".into()),
            });
        }
    }
}

/// What the turn's task reports to the engine.
#[derive(Debug)]
pub enum TurnUpdate {
    /// The sandbox is busy with another command; the turn is queued.
    Waiting,
    /// `claude` started; its stdin (for permission answers).
    Spawned(StdinTx),
    /// Claude Code's session is up (`system/init`): the session void should resume next time.
    Session { session: Uuid, model: String },
    /// The session couldn't be resumed; starting a fresh one with the transcript.
    Restarting,
    /// Something worth showing that isn't an error (API retries, Claude Code compacting).
    Status(String),
    /// Main-thread stream content for [`Segments`].
    Event(CcEvent),
    /// The turn's result: `error` is the user-facing failure, None on success.
    Result { usage: Usage, error: Option<String> },
    /// The process is gone. `error` when the turn ended without a result (or never started).
    Exited { error: Option<String> },
}

/// One turn's process setup.
#[derive(Clone, Debug)]
pub struct TurnSpec {
    pub conversation: ConversationId,
    /// The resolved `claude` (visible inside the sandbox: it's under the read-only root).
    pub program: PathBuf,
    pub args: TurnArgs,
    /// The chat's overlay on the workspace; None for Scratch.
    pub overlay: Option<OverlayMounts>,
    pub cwd: PathBuf,
    /// The home whose `.claude/` and `.claude.json` Claude Code uses.
    pub home: PathBuf,
    pub api_key: ApiKeySource,
    /// The user message to send.
    pub text: String,
    /// Sent instead when the session has to be started afresh (the transcript prepended).
    pub fresh_text: String,
}

/// How the turn's task reports to the engine.
pub type Report<'a> = &'a (dyn Fn(TurnUpdate) + Send + Sync);

enum Attempt {
    /// Claude Code doesn't have the session (or already has one by that id): start afresh.
    Restart(String),
    /// Over; the error when it ended without a result.
    End(Option<String>),
}

/// Run one turn to the end, reporting through `send`. Cancelling kills the process.
///
/// Must run as a task on the engine's runtime (not `spawn_blocking`): see [`Sandbox::spawn`].
pub async fn run_turn(
    sandbox: Arc<Sandbox>,
    spec: TurnSpec,
    bridge: Arc<dyn PermissionBridge>,
    cancel: CancellationToken,
    send: Report<'_>,
) {
    let key = match resolve_api_key(&spec.api_key).await {
        Ok(k) => k,
        Err(e) => {
            send(TurnUpdate::Exited {
                error: Some(format!("Couldn't get the API key for Claude Code: {e}")),
            });
            return;
        }
    };
    let mut args = spec.args.clone();
    let mut text = spec.text.clone();
    let mut restarted = false;
    loop {
        let a = attempt(
            &sandbox,
            &spec,
            &args,
            &text,
            key.as_deref(),
            !restarted,
            &bridge,
            &cancel,
            send,
        )
        .await;
        match a {
            Attempt::Restart(_) if !restarted => {
                restarted = true;
                args.session = SessionMode::New(Uuid::now_v7());
                text = spec.fresh_text.clone();
                send(TurnUpdate::Restarting);
            }
            Attempt::Restart(e) => {
                send(TurnUpdate::Exited { error: Some(e) });
                return;
            }
            Attempt::End(error) => {
                send(TurnUpdate::Exited { error });
                return;
            }
        }
    }
}

fn session_of(args: &TurnArgs) -> Uuid {
    match args.session {
        SessionMode::New(u) | SessionMode::Resume(u) => u,
    }
}

/// Claude Code's messages for a session id it can't use, lowercased: each entry is the
/// fragments one line must all contain. **Unverified** against the real CLI (2.1.330) until
/// the live smoke test: `--resume` of an unknown id ("No conversation found with session ID:
/// <id>") and `--session-id` of an existing one ("Session ID <id> is already in use").
const SESSION_ERRORS: &[&[&str]] = &[
    &["no conversation found with session id"],
    &["session id", "already in use"],
];

/// Claude Code saying it can't use the session id it was given (checked line by line, so
/// unrelated output that mentions a session elsewhere doesn't count).
fn session_unusable(text: &str) -> bool {
    text.lines().any(|line| {
        let l = line.to_lowercase();
        SESSION_ERRORS
            .iter()
            .any(|parts| parts.iter().all(|p| l.contains(p)))
    })
}

/// `text` with the API key (if any) blanked out.
fn redact(text: &str, key: Option<&str>) -> String {
    match key.filter(|k| !k.is_empty()) {
        Some(k) => text.replace(k, "[API key]"),
        None => text.to_string(),
    }
}

fn auth_failure(text: &str) -> bool {
    let t = text.to_lowercase();
    [
        "not logged in",
        "authentication_failed",
        "authentication failed",
        "/login",
        "invalid api key",
        "oauth token",
    ]
    .iter()
    .any(|k| t.contains(k))
}

#[allow(clippy::too_many_arguments)]
async fn attempt(
    sandbox: &Sandbox,
    spec: &TurnSpec,
    args: &TurnArgs,
    text: &str,
    key: Option<&str>,
    may_restart: bool,
    bridge: &Arc<dyn PermissionBridge>,
    cancel: &CancellationToken,
    send: Report<'_>,
) -> Attempt {
    let mut argv: Vec<OsString> = vec![spec.program.clone().into_os_string()];
    argv.extend(build_args(args).into_iter().map(OsString::from));
    let env = key
        .map(|k| vec![("ANTHROPIC_API_KEY".into(), k.into())])
        .unwrap_or_default();
    let (state, state_file) = (spec.home.join(".claude"), spec.home.join(".claude.json"));
    let plan = SpawnPlan {
        overlay: spec.overlay.clone(),
        argv,
        env,
        rw_binds: vec![
            RwBind::Dir(state.clone()),
            RwBind::File {
                path: state_file.clone(),
                if_missing: "{}".into(),
            },
        ],
        unhide: vec![state, state_file],
        cwd: spec.cwd.clone(),
    };
    let spawned = tokio::select! {
        s = sandbox.spawn(plan, || send(TurnUpdate::Waiting)) => s,
        _ = cancel.cancelled() => return Attempt::End(None),
    };
    let mut sp = match spawned {
        Ok(s) => s,
        Err(e) => return Attempt::End(Some(format!("Couldn't start Claude Code: {e}"))),
    };
    let (Some(stdin), Some(stdout), Some(stderr)) = (
        sp.child.stdin.take(),
        sp.child.stdout.take(),
        sp.child.stderr.take(),
    ) else {
        let _ = sp.kill().await;
        return Attempt::End(Some("Couldn't talk to Claude Code (no pipes)".into()));
    };
    let (tx, rx) = mpsc::unbounded_channel::<String>();
    let close = CancellationToken::new();
    tokio::spawn(write_stdin(stdin, rx, close.clone()));
    let stderr_task = tokio::spawn(stderr_tail(stderr));
    send(TurnUpdate::Spawned(tx.clone()));
    let id = Uuid::now_v7().simple().to_string();
    let request_id = format!("req_1_{}", &id[id.len() - 8..]);
    let _ = tx.send(initialize_request(&request_id));
    let _ = tx.send(user_message(text));

    let session = session_of(args);
    let mut reader = BufReader::new(stdout);
    let mut buf = Vec::new();
    let mut auth = false;
    // Real work happened (streamed, stored, maybe files changed): never start over after it.
    let mut worked = false;
    let mut result: Option<(Usage, bool, Option<String>)> = None;
    while result.is_none() {
        buf.clear();
        let n = tokio::select! {
            r = reader.read_until(b'\n', &mut buf) => r.unwrap_or(0),
            _ = cancel.cancelled() => {
                close.cancel();
                let _ = sp.kill().await;
                return Attempt::End(None);
            }
        };
        if n == 0 {
            break;
        }
        for ev in parse_events(&String::from_utf8_lossy(&buf)) {
            match ev {
                CcEvent::Init { session: id, model } => {
                    // The id Claude Code reports, when it's a UUID; else the one void passed.
                    let session = Uuid::parse_str(&id).unwrap_or(session);
                    send(TurnUpdate::Session { session, model })
                }
                CcEvent::PermissionRequest {
                    request_id,
                    tool,
                    input,
                    suggestions,
                    title,
                    description,
                    blocked_path,
                } => bridge.ask(
                    spec.conversation,
                    PermissionAsk {
                        request_id,
                        tool,
                        input,
                        suggestions,
                        title,
                        description,
                        blocked_path,
                    },
                    tx.clone(),
                ),
                CcEvent::ControlCancel { request_id } => {
                    bridge.cancel(spec.conversation, &request_id)
                }
                CcEvent::UnsupportedControl {
                    request_id,
                    subtype,
                } => {
                    let msg = format!("Unsupported control request subtype: {subtype}");
                    let _ = tx.send(control_response_error(&request_id, &msg));
                }
                CcEvent::ApiRetry { attempt, error } => {
                    let why = if error.is_empty() {
                        String::new()
                    } else {
                        format!(": {error}")
                    };
                    send(TurnUpdate::Status(format!(
                        "Claude Code is retrying the API (attempt {attempt}){why}…"
                    )));
                }
                CcEvent::CompactBoundary => send(TurnUpdate::Status(
                    "Claude Code compacted the conversation to save context.".into(),
                )),
                CcEvent::Result {
                    usage,
                    is_error,
                    message,
                    ..
                } => {
                    result = Some((usage, is_error, message));
                    break;
                }
                CcEvent::ControlResponse { .. } | CcEvent::SessionState(_) | CcEvent::Other => {}
                ev @ CcEvent::Assistant { .. } => {
                    if let CcEvent::Assistant {
                        error: Some(e),
                        parent: None,
                        ..
                    } = &ev
                    {
                        auth |= e == "authentication_failed";
                    }
                    worked |= matches!(&ev, CcEvent::Assistant { parent: None, .. });
                    send(TurnUpdate::Event(ev));
                }
                ev => {
                    worked |= matches!(
                        &ev,
                        CcEvent::TextDelta { parent: None, .. }
                            | CcEvent::ThinkingDelta { parent: None, .. }
                            | CcEvent::ToolResult { parent: None, .. }
                    );
                    send(TurnUpdate::Event(ev))
                }
            }
        }
    }

    match result {
        Some((usage, is_error, message)) => {
            let message = redact(&message.unwrap_or_default(), key);
            let restart = is_error && may_restart && !worked && session_unusable(&message);
            if !restart {
                let error = is_error.then(|| {
                    if auth || auth_failure(&message) {
                        not_signed_in(&message)
                    } else if message.is_empty() {
                        "Claude Code reported an error".to_string()
                    } else {
                        message.clone()
                    }
                });
                send(TurnUpdate::Result { usage, error });
            }
            // The turn is over: close stdin and let `claude` exit (it should, right away).
            close.cancel();
            let drain = async {
                while reader
                    .read_until(b'\n', &mut buf)
                    .await
                    .is_ok_and(|n| n > 0)
                {
                    buf.clear();
                }
                sp.wait().await
            };
            tokio::select! {
                r = tokio::time::timeout(EXIT_GRACE, drain) => if r.is_err() {
                    let _ = sp.kill().await;
                },
                _ = cancel.cancelled() => {
                    let _ = sp.kill().await;
                }
            }
            if restart {
                Attempt::Restart(message)
            } else {
                Attempt::End(None)
            }
        }
        None => {
            close.cancel();
            let status = tokio::select! {
                s = sp.wait() => s.ok(),
                _ = cancel.cancelled() => {
                    let _ = sp.kill().await;
                    return Attempt::End(None);
                }
            };
            let lines: Vec<String> =
                match tokio::time::timeout(Duration::from_secs(2), stderr_task).await {
                    Ok(Ok(l)) => l.into(),
                    _ => Vec::new(),
                };
            let stderr = redact(&lines.join("\n"), key);
            if may_restart && !worked && session_unusable(&stderr) {
                return Attempt::Restart(stderr);
            }
            if auth || auth_failure(&stderr) {
                return Attempt::End(Some(NOT_SIGNED_IN.into()));
            }
            let how = status.map(|s| format!(" ({s})")).unwrap_or_default();
            let mut e = format!("Claude Code stopped without finishing the reply{how}.");
            if !stderr.is_empty() {
                e.push('\n');
                e.push_str(&stderr);
            }
            Attempt::End(Some(e))
        }
    }
}

/// Writes queued lines to `claude`'s stdin until `close` (lines queued before it go first).
async fn write_stdin(
    mut stdin: ChildStdin,
    mut rx: mpsc::UnboundedReceiver<String>,
    close: CancellationToken,
) {
    loop {
        tokio::select! {
            biased;
            line = rx.recv() => {
                let Some(line) = line else { return };
                let ok = stdin.write_all(line.as_bytes()).await.is_ok()
                    && stdin.write_all(b"\n").await.is_ok()
                    && stdin.flush().await.is_ok();
                if !ok {
                    return;
                }
            }
            _ = close.cancelled() => return,
        }
    }
}

/// The last lines of stderr (for failure messages only; never logged).
async fn stderr_tail(err: impl AsyncRead + Unpin) -> VecDeque<String> {
    let mut reader = BufReader::new(err);
    let mut lines = VecDeque::new();
    let mut buf = Vec::new();
    while reader
        .read_until(b'\n', &mut buf)
        .await
        .is_ok_and(|n| n > 0)
    {
        let line = String::from_utf8_lossy(&buf);
        let line: String = line.trim_end().chars().take(STDERR_LINE_CAP).collect();
        buf.clear();
        if line.is_empty() {
            continue;
        }
        if lines.len() == STDERR_LINES {
            lines.pop_front();
        }
        lines.push_back(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Workspace;

    fn conv(msgs: Vec<Message>) -> Conversation {
        let mut c = Conversation::new(Workspace::scratch().id);
        c.messages = msgs;
        c
    }

    #[test]
    fn not_signed_in_names_the_fix() {
        assert_eq!(not_signed_in("  "), NOT_SIGNED_IN);
        assert!(NOT_SIGNED_IN.contains("`claude auth login`"));
        assert_eq!(
            not_signed_in("Not logged in · Please run /login\n"),
            "Claude Code isn't signed in — run `claude auth login` in a terminal \
             (Claude Code said: Not logged in · Please run /login)"
        );
        let long = not_signed_in(&"x".repeat(5000));
        assert!(
            long.len() < NOT_SIGNED_IN.len() + STDERR_LINE_CAP + 40,
            "{}",
            long.len()
        );
    }

    #[test]
    fn resolve_command_paths_and_home_bin() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let bin = home.join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let exe = bin.join("void-test-claude-xyz");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(resolve_command("void-test-claude-xyz", home).unwrap(), exe);
        assert_eq!(
            resolve_command("~/.local/bin/void-test-claude-xyz", home).unwrap(),
            exe
        );
        assert_eq!(resolve_command(exe.to_str().unwrap(), home).unwrap(), exe);
        assert_eq!(
            resolve_command("sh", home).unwrap().file_name().unwrap(),
            "sh"
        );
        let e = resolve_command("nope-not-here", home).unwrap_err();
        assert!(e.contains("`nope-not-here` not found"), "{e}");
        assert!(
            e.contains("install Claude Code or set the full path"),
            "{e}"
        );
        let plain = home.join("plain");
        std::fs::write(&plain, "x").unwrap();
        assert!(resolve_command(plain.to_str().unwrap(), home).is_err());
    }

    #[test]
    fn transcript_starts_at_latest_summary_and_is_capped() {
        let c = conv(vec![
            Message::user("ancient"),
            Message::new(Role::Assistant, vec![Part::Summary("the gist".into())]),
            Message::user("q"),
            Message::new(
                Role::Assistant,
                vec![
                    Part::Reasoning("hidden".into()),
                    Part::Text("a".into()),
                    Part::ToolCall {
                        id: "t".into(),
                        name: "Bash".into(),
                        arguments: "{\"command\":\"ls\"}".into(),
                    },
                ],
            ),
            Message::new(
                Role::Tool,
                vec![Part::ToolResult {
                    call_id: "t".into(),
                    content: "out".into(),
                }],
            ),
            Message::user("now"),
        ]);
        let t = transcript(&c, 5);
        assert!(
            !t.contains("ancient") && !t.contains("hidden") && !t.contains("now"),
            "{t}"
        );
        assert!(
            t.starts_with("Summary of the earlier conversation:\nthe gist"),
            "{t}"
        );
        assert!(
            t.contains("User: q\n\nAssistant: a\n\nAssistant used the tool Bash"),
            "{t}"
        );
        assert!(t.ends_with("Tool result:\nout"), "{t}");
        assert_eq!(with_transcript(&conv(vec![]), 0, "m"), "m");

        let big = conv(vec![
            Message::user(format!("OLDEST{}", "x".repeat(150 * 1024))),
            Message::user(format!("{}NEWEST", "é".repeat(60 * 1024))),
        ]);
        let t = transcript(&big, 2);
        assert!(t.len() <= TRANSCRIPT_CAP + 64);
        assert!(t.starts_with("[… earlier messages omitted]"));
        assert!(!t.contains("OLDEST") && t.ends_with("NEWEST"));
    }

    #[test]
    fn segments_split_at_tool_results() {
        let mut s = Segments::default();
        assert_eq!(s.start(), [Out::StreamStarted]);
        let out = s.on_event(CcEvent::TextDelta {
            text: "Hi".into(),
            parent: None,
        });
        assert_eq!(out, [Out::Delta(DeltaKind::Text, "Hi".into())]);
        let call = Part::ToolCall {
            id: "t1".into(),
            name: "Read".into(),
            arguments: "{\"file_path\":\"a.rs\"}".into(),
        };
        s.on_event(CcEvent::Assistant {
            parts: vec![Part::Text("Hi".into())],
            parent: None,
            error: None,
            usage: None,
        });
        let out = s.on_event(CcEvent::Assistant {
            parts: vec![call.clone()],
            parent: None,
            error: None,
            usage: Some(Usage {
                prompt_tokens: 50,
                completion_tokens: 5,
            }),
        });
        assert_eq!(
            out,
            [Out::ToolStarted {
                call_id: "t1".into(),
                name: "Read".into(),
                summary: "a.rs".into()
            }]
        );
        // A subagent's work goes to the parent card only.
        let out = s.on_event(CcEvent::Assistant {
            parts: vec![call.clone()],
            parent: Some("t1".into()),
            error: None,
            usage: Some(Usage {
                prompt_tokens: 9999,
                completion_tokens: 1,
            }),
        });
        assert_eq!(
            out,
            [Out::ToolOutput {
                call_id: "t1".into(),
                text: "› Read a.rs\n".into()
            }]
        );
        let out = s.on_event(CcEvent::ToolResult {
            call_id: "t1".into(),
            content: "code".into(),
            is_error: false,
            parent: None,
        });
        match &out[..] {
            [
                Out::Finished { message, .. },
                Out::ToolFinished { call_id, .. },
            ] => {
                assert_eq!(message.parts, [Part::Text("Hi".into()), call]);
                assert_eq!(message.usage.unwrap().prompt_tokens, 50);
                assert_eq!(call_id, "t1");
            }
            o => panic!("{o:?}"),
        }
        let out = s.on_event(CcEvent::TextDelta {
            text: "Do".into(),
            parent: None,
        });
        assert_eq!(out[0], Out::StreamStarted);
        // Stopped mid-stream: the unconfirmed delta text is kept.
        match s.finish(true, None, None) {
            Some(Out::Finished { message, .. }) => {
                assert!(message.stopped);
                assert_eq!(message.text(), "Do");
                assert_eq!(message.usage, None, "an earlier call's usage isn't reused");
            }
            o => panic!("{o:?}"),
        }
        assert_eq!(s.finish(false, None, None), None);
        match s.finish(false, Some("boom".into()), None) {
            Some(Out::Finished { message, .. }) => {
                assert_eq!(message.error.as_deref(), Some("boom"))
            }
            o => panic!("{o:?}"),
        }
    }

    /// Ending a turn finishes its still-open tool calls (as stopped), after the message that
    /// holds them; calls whose results came are left alone. A reset hands back its open calls.
    #[test]
    fn end_finishes_open_tool_calls() {
        let call = |id: &str| Part::ToolCall {
            id: id.into(),
            name: "Bash".into(),
            arguments: "{}".into(),
        };
        let mut s = Segments::default();
        s.start();
        s.on_event(CcEvent::Assistant {
            parts: vec![call("a"), call("b")],
            parent: None,
            error: None,
            usage: None,
        });
        s.on_event(CcEvent::ToolResult {
            call_id: "a".into(),
            content: "ok".into(),
            is_error: false,
            parent: None,
        });
        let out = s.end(true, None, None);
        assert_eq!(
            out,
            [Out::ToolFinished {
                call_id: "b".into(),
                content: STOPPED_TOOL.into(),
                is_error: true,
            }]
        );
        assert!(s.end(true, None, None).is_empty(), "finished once");

        let mut s = Segments::default();
        s.start();
        s.on_event(CcEvent::Assistant {
            parts: vec![Part::Text("x".into()), call("c")],
            parent: None,
            error: None,
            usage: None,
        });
        match &s.end(false, Some("boom".into()), None)[..] {
            [
                Out::Finished { message, .. },
                Out::ToolFinished {
                    call_id, is_error, ..
                },
            ] => {
                assert_eq!(message.parts.len(), 2);
                assert_eq!(call_id, "c");
                assert!(is_error);
            }
            o => panic!("{o:?}"),
        }

        let mut s = Segments::default();
        s.start();
        s.on_event(CcEvent::Assistant {
            parts: vec![call("d")],
            parent: None,
            error: None,
            usage: None,
        });
        assert_eq!(s.reset(), ["d".to_string()]);
        assert!(
            s.end(false, None, None)
                .iter()
                .all(|o| !matches!(o, Out::ToolFinished { .. }))
        );
    }

    #[test]
    fn result_usage_only_without_per_call_usage() {
        let mut s = Segments::default();
        s.start();
        let u = Usage {
            prompt_tokens: 7,
            completion_tokens: 2,
        };
        match s.finish(false, None, Some(u)) {
            Some(Out::Finished { message, .. }) => assert_eq!(message.usage, Some(u)),
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn error_detection() {
        assert!(session_unusable(
            "No conversation found with session ID: 0192-..."
        ));
        assert!(session_unusable("Error: Session ID abc is already in use."));
        assert!(!session_unusable("rate limited"));
        // Unrelated noise that merely mentions a session isn't Claude Code's session error.
        assert!(!session_unusable("mcp server \"notes\": session not found"));
        assert!(!session_unusable("warning: session\nsomething not found"));
        assert!(session_unusable(
            "some warning\nNo conversation found with session ID: x"
        ));
        assert!(auth_failure("Not logged in · Please run /login"));
        assert!(auth_failure("Invalid API key · Fix external API key"));
        assert!(!auth_failure("Prompt is too long"));
    }

    #[tokio::test]
    async fn backstop_ends_a_panicking_turn() {
        let (tx, mut rx) = mpsc::unbounded_channel::<TurnUpdate>();
        let task = tokio::spawn(async move {
            let b = Backstop::new(move |u| {
                let _ = tx.send(u);
            });
            b.send(TurnUpdate::Waiting);
            if b.exited.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            panic!("turn task bug");
        });
        assert!(task.await.is_err(), "the task panicked");
        assert!(matches!(rx.recv().await, Some(TurnUpdate::Waiting)));
        match rx.recv().await {
            Some(TurnUpdate::Exited { error: Some(e) }) => {
                assert!(e.contains("unexpectedly"), "{e}")
            }
            u => panic!("{u:?}"),
        }
        assert!(rx.recv().await.is_none());
    }

    #[test]
    fn backstop_stays_quiet_after_exited() {
        let (tx, mut rx) = mpsc::unbounded_channel::<TurnUpdate>();
        {
            let b = Backstop::new(move |u| {
                let _ = tx.send(u);
            });
            b.send(TurnUpdate::Exited { error: None });
        }
        assert!(matches!(
            rx.try_recv(),
            Ok(TurnUpdate::Exited { error: None })
        ));
        assert!(rx.try_recv().is_err(), "one Exited only");
    }
}
