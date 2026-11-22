//! The engine: owns conversations, config and streams on its own tokio runtime, and talks to
//! any UI through commands in and events out. Nothing here knows about a particular UI.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use chrono::Utc;
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use serde_json::Value;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::agent;
use crate::backend::{Backend, BackendError, StreamEvent, make_backend};
use crate::config::{Config, Profile, load_config, resolve_api_key, save_config};
use crate::context::{build_request, estimate_tokens};
use crate::context::{compaction_split, needs_compaction};
use crate::model::{
    Conversation, ConversationId, ConversationMeta, Message, NEW_CHAT_TITLE, Part, Role, Workspace,
    WorkspaceId,
};
use crate::overlay::{Change, Hunk, Overlay};
use crate::permissions::{Action, ApprovalRequest, Decision, Permissions};
use crate::sandbox::{Availability, Sandbox};
use crate::store::{Store, WorkspaceEntry};
use crate::tools::{Registry, ToolCtx, ToolOutput};

/// Called after every event, so a UI can wake up (the GUI requests a repaint).
pub type Notify = Arc<dyn Fn() + Send + Sync>;
/// Builds the backend for a profile and its resolved key (tests swap in fakes).
pub type BackendFactory = Arc<dyn Fn(&Profile, Option<String>) -> Arc<dyn Backend> + Send + Sync>;

#[derive(Clone, Debug)]
pub enum Command {
    /// None opens Scratch.
    OpenWorkspace(Option<PathBuf>),
    NewConversation,
    OpenConversation(ConversationId),
    RenameConversation(ConversationId, String),
    DeleteConversation(ConversationId),
    /// None follows the active profile.
    SetConversationProfile(ConversationId, Option<String>),
    SendMessage {
        conversation: ConversationId,
        text: String,
    },
    /// Ask again for the reply to the last user message (after a failed or stopped reply).
    Retry(ConversationId),
    Stop(ConversationId),
    SaveConfig(Config),
    TestProfile(Profile),
    /// None follows the default agent.
    SetConversationAgent(ConversationId, Option<String>),
    /// Answer an `ApprovalNeeded`.
    Approve {
        id: u64,
        decision: Decision,
    },
    ListChanges(ConversationId),
    Diff {
        conversation: ConversationId,
        path: PathBuf,
    },
    AcceptFile {
        conversation: ConversationId,
        path: PathBuf,
        force: bool,
    },
    RejectFile {
        conversation: ConversationId,
        path: PathBuf,
    },
    AcceptHunk {
        conversation: ConversationId,
        path: PathBuf,
        index: usize,
        force: bool,
    },
    RejectHunk {
        conversation: ConversationId,
        path: PathBuf,
        index: usize,
    },
    AcceptAll {
        conversation: ConversationId,
        keep_ignored: bool,
    },
    RejectAll(ConversationId),
    /// Summarise the conversation now.
    Compact(ConversationId),
    ClearSandboxCaches,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeltaKind {
    Text,
    Reasoning,
}

#[derive(Clone, Debug)]
pub enum Event {
    ConfigLoaded {
        config: Config,
        warning: Option<String>,
    },
    WorkspaceOpened {
        workspace: Workspace,
        conversations: Vec<ConversationMeta>,
        recent: Vec<WorkspaceEntry>,
        layout: Option<String>,
        warnings: Vec<String>,
    },
    ConversationList(Vec<ConversationMeta>),
    /// A conversation made by `NewConversation`.
    ConversationCreated(Conversation),
    /// The full, current state of one conversation.
    Conversation(Conversation),
    ConversationDeleted(ConversationId),
    StreamStarted(ConversationId),
    Delta {
        conversation: ConversationId,
        kind: DeltaKind,
        text: String,
    },
    StreamFinished {
        conversation: ConversationId,
        message: Message,
        tokens_per_sec: Option<f32>,
    },
    StreamFailed {
        conversation: ConversationId,
        error: String,
    },
    ProfileTested {
        profile_id: String,
        result: Result<Vec<String>, String>,
    },
    ConfigSaved,
    Error(String),
    /// Something worth telling the user that isn't an error.
    Info(String),
    SandboxStatus(Availability),
    ToolStarted {
        conversation: ConversationId,
        call_id: String,
        name: String,
        summary: String,
    },
    ToolOutput {
        conversation: ConversationId,
        call_id: String,
        text: String,
    },
    ToolFinished {
        conversation: ConversationId,
        call_id: String,
        output: ToolOutput,
    },
    ApprovalNeeded(ApprovalRequest),
    ApprovalResolved {
        id: u64,
    },
    /// The conversation's pending changes (after each tool round and review action).
    Changes {
        conversation: ConversationId,
        changes: Vec<Change>,
    },
    Diff {
        conversation: ConversationId,
        path: PathBuf,
        hunks: Vec<Hunk>,
    },
    ReviewError {
        conversation: ConversationId,
        path: PathBuf,
        error: String,
    },
    /// Older messages were summarised.
    Compacted(ConversationId),
}

/// Knobs for embedding the engine (tests lower the step limit and skip the sandbox).
#[derive(Clone, Copy, Debug)]
pub struct EngineOptions {
    /// Tool rounds per turn before the agent is stopped.
    pub step_limit: u32,
    /// Set up the bash sandbox (false: bash reports it's unavailable).
    pub sandbox: bool,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            step_limit: 50,
            sandbox: true,
        }
    }
}

pub struct Engine;

impl Engine {
    pub fn start(config_path: PathBuf, store: Store, notify: Notify) -> EngineHandle {
        let factory: BackendFactory = Arc::new(make_backend);
        Self::start_with(config_path, store, notify, factory)
    }

    pub fn start_with(
        config_path: PathBuf,
        store: Store,
        notify: Notify,
        factory: BackendFactory,
    ) -> EngineHandle {
        Self::start_with_options(
            config_path,
            store,
            notify,
            factory,
            EngineOptions::default(),
        )
    }

    pub fn start_with_options(
        config_path: PathBuf,
        store: Store,
        notify: Notify,
        factory: BackendFactory,
        options: EngineOptions,
    ) -> EngineHandle {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("void-engine")
            .enable_all()
            .build()
            .expect("couldn't start the engine runtime");
        let (tx, rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = std_mpsc::channel();
        let state = State {
            config_path,
            config: Config::default(),
            store,
            factory,
            notify,
            events: ev_tx,
            tx: tx.clone(),
            workspace: Workspace::scratch(),
            convs: HashMap::new(),
            streams: HashMap::new(),
            next_gen: 0,
            registry: Registry::builtin(),
            sandbox: None,
            sandbox_status: Availability::Unavailable("not checked yet".into()),
            turns: HashMap::new(),
            approvals: HashMap::new(),
            next_approval: 1,
            options,
        };
        runtime.spawn(state.run(rx));
        EngineHandle {
            tx,
            events: ev_rx,
            runtime: Some(runtime),
        }
    }
}

/// The UI's side of the engine.
pub struct EngineHandle {
    tx: mpsc::UnboundedSender<Msg>,
    events: std_mpsc::Receiver<Event>,
    runtime: Option<tokio::runtime::Runtime>,
}

impl EngineHandle {
    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(Msg::Cmd(cmd));
    }

    pub fn try_recv(&self) -> Option<Event> {
        self.events.try_recv().ok()
    }

    /// Save a workspace's dock layout (an opaque string owned by the UI).
    pub fn save_layout(&self, ws: &WorkspaceId, json: String) {
        let _ = self.tx.send(Msg::SaveLayout(ws.clone(), json));
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        // Let in-flight replies be saved as stopped before the runtime goes away.
        let (ack, done) = std_mpsc::channel();
        if self.tx.send(Msg::Shutdown(ack)).is_ok() {
            let _ = done.recv_timeout(Duration::from_secs(2));
        }
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_timeout(Duration::from_secs(1));
        }
    }
}

enum Msg {
    Cmd(Command),
    /// Stop every stream (keeping partial replies), then acknowledge.
    Shutdown(std_mpsc::Sender<()>),
    SaveLayout(WorkspaceId, String),
    Stream {
        conversation: ConversationId,
        generation: u64,
        item: StreamItem,
    },
    Tool(ConversationId, ToolMsg),
    Approval {
        req: ApprovalRequest,
        reply: oneshot::Sender<Decision>,
    },
    AddRule {
        ws: WorkspaceId,
        tool: String,
        prefix: String,
    },
}

enum ToolMsg {
    Started {
        call_id: String,
        name: String,
        summary: String,
    },
    Output {
        call_id: String,
        text: String,
    },
    Finished {
        call_id: String,
        output: ToolOutput,
    },
    /// Every call of the round has a result.
    RoundDone,
}

#[derive(Clone, Copy, PartialEq)]
enum StreamMode {
    Reply,
    /// Summarise, then stop (Command::Compact).
    CompactOnly,
}

/// An agent turn: from the user's message until a reply without tool calls.
struct Turn {
    round: u32,
    cancel: CancellationToken,
    runner: Option<AbortHandle>,
    workspace: Workspace,
}

enum StreamItem {
    Event(StreamEvent),
    Failed(String),
    /// Compaction produced a summary to insert before message `split`.
    Summary {
        split: usize,
        text: String,
    },
    CompactOnlyDone,
    /// The backend's stream ended without saying Done.
    Ended,
}

struct Streaming {
    generation: u64,
    abort: AbortHandle,
    message: Message,
    first_delta: Option<Instant>,
}

struct State {
    config_path: PathBuf,
    config: Config,
    store: Store,
    factory: BackendFactory,
    notify: Notify,
    events: std_mpsc::Sender<Event>,
    tx: mpsc::UnboundedSender<Msg>,
    workspace: Workspace,
    /// Loaded conversations: the current workspace's, plus any still streaming elsewhere.
    convs: HashMap<ConversationId, Conversation>,
    streams: HashMap<ConversationId, Streaming>,
    next_gen: u64,
    registry: Registry,
    sandbox: Option<Arc<Sandbox>>,
    sandbox_status: Availability,
    turns: HashMap<ConversationId, Turn>,
    approvals: HashMap<u64, (ConversationId, oneshot::Sender<Decision>)>,
    next_approval: u64,
    options: EngineOptions,
}

const TITLE_CHARS: usize = 40;

impl State {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>) {
        let (config, warning) = load_config(&self.config_path);
        self.config = config.clone();
        self.emit(Event::ConfigLoaded { config, warning });
        let last = self.store.last_workspace();
        self.open_workspace(last);
        if self.options.sandbox {
            let sb = self.make_sandbox();
            self.sandbox_status = sb.availability().await;
            self.sandbox = Some(Arc::new(sb));
        } else {
            self.sandbox_status = Availability::Unavailable("the sandbox is turned off".into());
        }
        self.emit(Event::SandboxStatus(self.sandbox_status.clone()));
        while let Some(msg) = rx.recv().await {
            match msg {
                Msg::Cmd(cmd) => self.command(cmd),
                Msg::Shutdown(ack) => {
                    let ids: Vec<_> = self.streams.keys().copied().collect();
                    for id in ids {
                        if let Some(s) = self.streams.get(&id) {
                            s.abort.abort();
                        }
                        self.finish(id, true);
                    }
                    let turns: Vec<_> = self.turns.keys().copied().collect();
                    for id in turns {
                        self.end_turn(id);
                    }
                    let _ = ack.send(());
                }
                Msg::SaveLayout(ws, json) => {
                    if let Err(e) = self.store.save_layout(&ws, &json) {
                        self.emit(Event::Error(format!("Couldn't save the layout: {e}")));
                    }
                }
                Msg::Stream {
                    conversation,
                    generation,
                    item,
                } => self.stream_item(conversation, generation, item),
                Msg::Tool(id, m) => self.tool_msg(id, m),
                Msg::Approval { mut req, reply } => {
                    if !self.turns.contains_key(&req.conversation) {
                        continue;
                    }
                    req.id = self.next_approval;
                    self.next_approval += 1;
                    self.approvals.insert(req.id, (req.conversation, reply));
                    self.emit(Event::ApprovalNeeded(req));
                }
                Msg::AddRule { ws, tool, prefix } => {
                    let path = self.store.permissions_path(&ws);
                    let (mut perms, _) = Permissions::load(&path);
                    perms.add_always(&tool, (!prefix.is_empty()).then_some(prefix));
                    if let Err(e) = perms.save(&path) {
                        self.emit(Event::Error(format!(
                            "Couldn't save the permission rule: {e}"
                        )));
                    }
                }
            }
        }
    }

    fn make_sandbox(&self) -> Sandbox {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        Sandbox::new(
            self.store.data_dir().to_path_buf(),
            home,
            self.config.sandbox.clone(),
        )
    }

    /// The conversation's overlay in `ws` (created when missing); None outside a folder.
    fn overlay_for(&self, ws: &Workspace, id: ConversationId) -> Option<Overlay> {
        let root = ws.root.as_ref()?;
        Overlay::open(&self.store.overlay_dir(&ws.id, &id), root).ok()
    }

    /// The overlay only if the conversation already has one.
    fn existing_overlay(&self, ws: &Workspace, id: ConversationId) -> Option<Overlay> {
        if self.store.overlay_dir(&ws.id, &id).exists() {
            self.overlay_for(ws, id)
        } else {
            None
        }
    }

    fn emit_changes(&self, ws: &Workspace, id: ConversationId) {
        if let Some(ov) = self.existing_overlay(ws, id) {
            self.emit(Event::Changes {
                conversation: id,
                changes: ov.changes(),
            });
        }
    }

    fn agent_for(&self, c: &Conversation) -> Option<agent::AgentProfile> {
        self.config.agent(c.agent.as_deref()).cloned()
    }

    fn begin_turn(&mut self, c: &Conversation) {
        let ws = self.workspace.clone();
        let tools = ws.root.is_some() && self.agent_for(c).is_some_and(|a| !a.tools.is_empty());
        if tools
            && let Some(ov) = self.overlay_for(&ws, c.id)
            && let Err(e) = ov.checkpoint()
        {
            self.emit(Event::Error(format!(
                "Couldn't checkpoint pending changes: {e}"
            )));
        }
        self.turns.insert(
            c.id,
            Turn {
                round: 0,
                cancel: CancellationToken::new(),
                runner: None,
                workspace: ws,
            },
        );
    }

    /// Stop a turn: cancel its tools, refuse its pending approvals, report its changes.
    fn end_turn(&mut self, id: ConversationId) {
        let Some(t) = self.turns.remove(&id) else {
            return;
        };
        t.cancel.cancel();
        if let Some(r) = t.runner {
            r.abort();
        }
        let pending: Vec<u64> = self
            .approvals
            .iter()
            .filter(|(_, (c, _))| *c == id)
            .map(|(k, _)| *k)
            .collect();
        for k in pending {
            self.approvals.remove(&k);
            self.emit(Event::ApprovalResolved { id: k });
        }
        self.emit_changes(&t.workspace, id);
    }

    fn tool_msg(&mut self, id: ConversationId, m: ToolMsg) {
        if !self.turns.contains_key(&id) {
            return;
        }
        match m {
            ToolMsg::Started {
                call_id,
                name,
                summary,
            } => {
                self.emit(Event::ToolStarted {
                    conversation: id,
                    call_id,
                    name,
                    summary,
                });
            }
            ToolMsg::Output { call_id, text } => {
                self.emit(Event::ToolOutput {
                    conversation: id,
                    call_id,
                    text,
                });
            }
            ToolMsg::Finished { call_id, output } => {
                let msg = Message::new(
                    Role::Tool,
                    vec![Part::ToolResult {
                        call_id: call_id.clone(),
                        content: output.content.clone(),
                    }],
                );
                self.append(id, msg);
                self.emit(Event::ToolFinished {
                    conversation: id,
                    call_id,
                    output,
                });
            }
            ToolMsg::RoundDone => {
                let Some(t) = self.turns.get_mut(&id) else {
                    return;
                };
                t.round += 1;
                t.runner = None;
                let ws = t.workspace.clone();
                self.emit_changes(&ws, id);
                if let Some(c) = self.convs.get(&id).cloned() {
                    self.start_stream(c, StreamMode::Reply);
                }
            }
        }
    }

    /// Run a reply's tool calls in order on a task of their own.
    fn spawn_tools(&mut self, id: ConversationId, calls: Vec<(String, String, String)>) {
        let Some(turn) = self.turns.get(&id) else {
            return;
        };
        let ws = turn.workspace.clone();
        let cancel = turn.cancel.clone();
        let agent = self.convs.get(&id).and_then(|c| self.agent_for(c));
        let overlay = self.overlay_for(&ws, id);
        let perms = Permissions::load(&self.store.permissions_path(&ws.id)).0;
        let hidden = Arc::new(
            self.sandbox
                .as_ref()
                .map(|s| s.hidden_paths())
                .unwrap_or_default(),
        );
        let sandbox = self.sandbox.clone();
        let target_dir = self.store.sandbox_target(&ws.id);
        let registry = self.registry.clone();
        let tx = self.tx.clone();
        let task = tokio::spawn(async move {
            let send = |m: ToolMsg| {
                let _ = tx.send(Msg::Tool(id, m));
            };
            let notes = overlay
                .as_ref()
                .map(Overlay::take_rejections)
                .unwrap_or_default();
            let mut first = true;
            for (call_id, name, arguments) in calls {
                if cancel.is_cancelled() {
                    return;
                }
                let tool = registry.get(&name);
                let args: Option<Value> = if arguments.trim().is_empty() {
                    Some(Value::Object(Default::default()))
                } else {
                    serde_json::from_str(&arguments)
                        .ok()
                        .filter(Value::is_object)
                };
                let summary = match (&tool, &args) {
                    (Some(t), Some(a)) => t.summary(a),
                    _ => String::new(),
                };
                send(ToolMsg::Started {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    summary: summary.clone(),
                });
                let mut output = 'out: {
                    let Some(t) = tool else {
                        break 'out ToolOutput::err(format!("there is no tool called `{name}`"));
                    };
                    let Some(a) = args else {
                        break 'out ToolOutput::err("the arguments weren't a JSON object");
                    };
                    let (Some(ov), Some(root)) = (overlay.clone(), ws.root.clone()) else {
                        break 'out ToolOutput::err(
                            "tools need a folder workspace (File › Open Folder)",
                        );
                    };
                    let Some(ag) = agent.as_ref() else {
                        break 'out ToolOutput::err("no agent profile is configured");
                    };
                    if !ag.allows(&name) {
                        break 'out ToolOutput::err(format!(
                            "the {} agent can't use {name}",
                            ag.id
                        ));
                    }
                    match perms.decide(&name, &a) {
                        Action::Allow => {}
                        Action::Deny => {
                            break 'out ToolOutput::err(format!(
                                "{name} is denied by a workspace rule"
                            ));
                        }
                        Action::Ask => {
                            let (otx, orx) = oneshot::channel();
                            let command =
                                a.get("command").and_then(Value::as_str).map(str::to_owned);
                            let req = ApprovalRequest {
                                id: 0,
                                conversation: id,
                                tool: name.clone(),
                                summary: summary.clone(),
                                command,
                            };
                            let _ = tx.send(Msg::Approval { req, reply: otx });
                            match orx.await {
                                Ok(Decision::Once) => {}
                                Ok(Decision::AlwaysForWorkspace(prefix)) => {
                                    let _ = tx.send(Msg::AddRule {
                                        ws: ws.id.clone(),
                                        tool: name.clone(),
                                        prefix,
                                    });
                                }
                                _ => break 'out ToolOutput::err("denied by the user"),
                            }
                        }
                    }
                    let (stx, mut srx) = tokio::sync::mpsc::unbounded_channel::<String>();
                    let fwd_tx = tx.clone();
                    let cid = call_id.clone();
                    let fwd = tokio::spawn(async move {
                        while let Some(text) = srx.recv().await {
                            let _ = fwd_tx.send(Msg::Tool(
                                id,
                                ToolMsg::Output {
                                    call_id: cid.clone(),
                                    text,
                                },
                            ));
                        }
                    });
                    let ctx = ToolCtx {
                        workspace: root,
                        overlay: ov,
                        hidden: hidden.clone(),
                        cancel: cancel.clone(),
                        stream: Some(stx),
                        sandbox: sandbox.clone(),
                        target_dir: target_dir.clone(),
                    };
                    let out = t.run(a, ctx).await;
                    let _ = fwd.await;
                    out
                };
                if first && !notes.is_empty() {
                    output.content = format!("{}\n\n{}", notes.join("\n"), output.content);
                }
                first = false;
                send(ToolMsg::Finished { call_id, output });
            }
            if !cancel.is_cancelled() {
                send(ToolMsg::RoundDone);
            }
        });
        if let Some(t) = self.turns.get_mut(&id) {
            t.runner = Some(task.abort_handle());
        }
    }

    /// A review action on a conversation's overlay in the current workspace.
    fn review(
        &mut self,
        id: ConversationId,
        path: PathBuf,
        f: impl FnOnce(&Overlay) -> Result<(), String>,
    ) {
        let ws = self.workspace.clone();
        let Some(ov) = self.existing_overlay(&ws, id) else {
            self.emit(Event::Changes {
                conversation: id,
                changes: Vec::new(),
            });
            return;
        };
        if let Err(error) = f(&ov) {
            self.emit(Event::ReviewError {
                conversation: id,
                path,
                error,
            });
        }
        self.emit(Event::Changes {
            conversation: id,
            changes: ov.changes(),
        });
    }

    fn emit(&self, ev: Event) {
        let _ = self.events.send(ev);
        (self.notify)();
    }

    fn command(&mut self, cmd: Command) {
        match cmd {
            Command::OpenWorkspace(None) => self.open_workspace(Workspace::scratch()),
            Command::OpenWorkspace(Some(path)) => match self.store.workspace_for(&path) {
                Ok(ws) => self.open_workspace(ws),
                Err(e) => self.emit(Event::Error(format!(
                    "Couldn't open {}: {e}",
                    path.display()
                ))),
            },
            Command::NewConversation => {
                let c = Conversation::new(self.workspace.id.clone());
                self.save(&c);
                self.convs.insert(c.id, c.clone());
                self.emit(Event::ConversationCreated(c));
                self.emit_list();
            }
            Command::OpenConversation(id) => {
                if let Some(c) = self.conversation(id).cloned() {
                    self.emit(Event::Conversation(c));
                }
            }
            Command::RenameConversation(id, title) => {
                let title = title.trim().to_string();
                if title.is_empty() {
                    return;
                }
                self.update(id, |c| c.title = title);
            }
            Command::DeleteConversation(id) => {
                if let Some(s) = self.streams.remove(&id) {
                    s.abort.abort();
                }
                self.end_turn(id);
                let ws = self
                    .convs
                    .remove(&id)
                    .map(|c| c.workspace_id)
                    .unwrap_or_else(|| self.workspace.id.clone());
                if let Err(e) = self.store.delete(&ws, &id) {
                    self.emit(Event::Error(format!(
                        "Couldn't delete the conversation: {e}"
                    )));
                }
                let overlay = self.store.overlay_dir(&ws, &id);
                if overlay.exists()
                    && let Err(e) = std::fs::remove_dir_all(&overlay)
                {
                    self.emit(Event::Error(format!(
                        "Couldn't discard the pending changes: {e}"
                    )));
                }
                self.emit(Event::ConversationDeleted(id));
                self.emit_list();
            }
            Command::SetConversationProfile(id, profile) => {
                self.update(id, |c| c.profile_id = profile);
            }
            Command::SendMessage { conversation, text } => self.send_message(conversation, text),
            Command::Retry(id) => {
                if self.streams.contains_key(&id) {
                    return;
                }
                let ws = self.workspace.clone();
                let Some(c) = self.conversation(id) else {
                    return;
                };
                if !c.messages.iter().any(|m| m.role == Role::User) {
                    return;
                }
                // A retry replaces everything after the last user message — replies, tool
                // results, and (via the turn's checkpoint) the attempt's file changes.
                while c
                    .messages
                    .last()
                    .is_some_and(|m| matches!(m.role, Role::Assistant | Role::Tool))
                {
                    c.messages.pop();
                }
                let c = c.clone();
                if let Some(ov) = self.existing_overlay(&ws, id)
                    && let Err(e) = ov.restore_checkpoint()
                {
                    self.emit(Event::Error(format!(
                        "Couldn't undo the last attempt's changes: {e}"
                    )));
                }
                self.save(&c);
                self.emit(Event::Conversation(c.clone()));
                self.emit_changes(&ws, id);
                self.begin_turn(&c);
                self.start_stream(c, StreamMode::Reply);
            }
            Command::Stop(id) => {
                if let Some(s) = self.streams.get(&id) {
                    s.abort.abort();
                    self.finish(id, true);
                }
                self.end_turn(id);
            }
            Command::SetConversationAgent(id, agent) => {
                self.update(id, |c| c.agent = agent);
            }
            Command::Approve { id, decision } => {
                if let Some((_, reply)) = self.approvals.remove(&id) {
                    let _ = reply.send(decision);
                }
                self.emit(Event::ApprovalResolved { id });
            }
            Command::ListChanges(id) => self.review(id, PathBuf::new(), |_| Ok(())),
            Command::Diff { conversation, path } => {
                let ws = self.workspace.clone();
                match self
                    .existing_overlay(&ws, conversation)
                    .map(|ov| ov.diff(&path))
                {
                    Some(Ok(hunks)) => self.emit(Event::Diff {
                        conversation,
                        path,
                        hunks,
                    }),
                    Some(Err(e)) => self.emit(Event::ReviewError {
                        conversation,
                        path,
                        error: e.to_string(),
                    }),
                    None => {}
                }
            }
            Command::AcceptFile {
                conversation,
                path,
                force,
            } => {
                let p = path.clone();
                self.review(conversation, path, move |ov| {
                    ov.accept_file(&p, force).map_err(|e| e.to_string())
                });
            }
            Command::RejectFile { conversation, path } => {
                let p = path.clone();
                self.review(conversation, path, move |ov| {
                    ov.reject_file(&p).map_err(|e| e.to_string())
                });
            }
            Command::AcceptHunk {
                conversation,
                path,
                index,
                force,
            } => {
                let p = path.clone();
                self.review(conversation, path, move |ov| {
                    ov.accept_hunk(&p, index, force).map_err(|e| e.to_string())
                });
            }
            Command::RejectHunk {
                conversation,
                path,
                index,
            } => {
                let p = path.clone();
                self.review(conversation, path, move |ov| {
                    ov.reject_hunk(&p, index).map_err(|e| e.to_string())
                });
            }
            Command::AcceptAll {
                conversation,
                keep_ignored,
            } => {
                self.review(conversation, PathBuf::new(), move |ov| {
                    match ov.accept_all(keep_ignored) {
                        Ok(skipped) if skipped.is_empty() => Ok(()),
                        Ok(skipped) => Err(format!(
                            "Not accepted because they changed on disk: {}",
                            skipped
                                .iter()
                                .map(|p| p.display().to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )),
                        Err(e) => Err(e.to_string()),
                    }
                });
            }
            Command::RejectAll(id) => {
                self.review(id, PathBuf::new(), |ov| {
                    ov.reject_all().map_err(|e| e.to_string())
                });
            }
            Command::Compact(id) => {
                if self.streams.contains_key(&id) || self.turns.contains_key(&id) {
                    self.emit(Event::Error(
                        "Wait for the reply to finish before compacting.".into(),
                    ));
                } else if let Some(c) = self.conversation(id).cloned() {
                    self.start_stream(c, StreamMode::CompactOnly);
                }
            }
            Command::ClearSandboxCaches => {
                if let Some(sb) = &self.sandbox {
                    let dir = sb.cache_layer_dir();
                    match std::fs::remove_dir_all(&dir) {
                        Ok(()) => self.emit(Event::Info("Sandbox caches cleared.".into())),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            self.emit(Event::Info("Sandbox caches were already empty.".into()))
                        }
                        Err(e) => self.emit(Event::Error(format!(
                            "Couldn't clear the sandbox caches: {e}"
                        ))),
                    }
                }
            }
            Command::SaveConfig(config) => match save_config(&self.config_path, &config) {
                Ok(()) => {
                    let sandbox_changed = config.sandbox != self.config.sandbox;
                    self.config = config;
                    if sandbox_changed && self.options.sandbox {
                        self.sandbox = Some(Arc::new(self.make_sandbox()));
                    }
                    self.emit(Event::ConfigSaved);
                }
                Err(e) => self.emit(Event::Error(format!("Couldn't save the settings: {e}"))),
            },
            Command::TestProfile(profile) => {
                let factory = self.factory.clone();
                let events = self.events.clone();
                let notify = self.notify.clone();
                tokio::spawn(async move {
                    let result = match resolve_api_key(&profile.api_key).await {
                        Err(e) => Err(e.to_string()),
                        Ok(key) => factory(&profile, key)
                            .list_models()
                            .await
                            .map_err(|e| e.to_string()),
                    };
                    let _ = events.send(Event::ProfileTested {
                        profile_id: profile.id.clone(),
                        result,
                    });
                    notify();
                });
            }
        }
    }

    fn open_workspace(&mut self, ws: Workspace) {
        if let Err(e) = self.store.touch(&ws) {
            self.emit(Event::Error(format!("Couldn't record the workspace: {e}")));
        }
        let streaming: Vec<_> = self.streams.keys().copied().collect();
        self.convs.retain(|id, _| streaming.contains(id));
        let (conversations, warnings) = self.store.list(&ws.id);
        let layout = self.store.load_layout(&ws.id);
        self.workspace = ws.clone();
        self.emit(Event::WorkspaceOpened {
            workspace: ws,
            conversations,
            recent: self.store.recent(),
            layout,
            warnings,
        });
    }

    fn emit_list(&self) {
        let (list, warnings) = self.store.list(&self.workspace.id);
        for w in warnings {
            self.emit(Event::Error(w));
        }
        self.emit(Event::ConversationList(list));
    }

    fn save(&self, c: &Conversation) {
        if let Err(e) = self.store.save(c) {
            self.emit(Event::Error(format!("Couldn't save the conversation: {e}")));
        }
    }

    /// A loaded conversation, loading it from the current workspace when needed.
    fn conversation(&mut self, id: ConversationId) -> Option<&mut Conversation> {
        if !self.convs.contains_key(&id) {
            match self.store.load(&self.workspace.id, &id) {
                Ok(c) => {
                    self.convs.insert(id, c);
                }
                Err(e) => {
                    self.emit(Event::Error(format!("Couldn't open the conversation: {e}")));
                    return None;
                }
            }
        }
        self.convs.get_mut(&id)
    }

    fn update(&mut self, id: ConversationId, f: impl FnOnce(&mut Conversation)) {
        let Some(c) = self.conversation(id) else {
            return;
        };
        f(c);
        c.updated = Utc::now();
        let c = c.clone();
        self.save(&c);
        self.emit(Event::Conversation(c));
        self.emit_list();
    }

    fn send_message(&mut self, id: ConversationId, text: String) {
        if self.streams.contains_key(&id) {
            self.emit(Event::Error(
                "This conversation is already replying; stop it first.".into(),
            ));
            return;
        }
        let Some(c) = self.conversation(id) else {
            return;
        };
        if c.title == NEW_CHAT_TITLE && !c.messages.iter().any(|m| m.role == Role::User) {
            let first_line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
            let title: String = first_line.trim().chars().take(TITLE_CHARS).collect();
            if !title.is_empty() {
                c.title = title;
            }
        }
        c.messages.push(Message::user(text));
        c.updated = Utc::now();
        let c = c.clone();
        self.save(&c);
        self.emit(Event::Conversation(c.clone()));
        self.emit_list();
        self.begin_turn(&c);
        self.start_stream(c, StreamMode::Reply);
    }

    /// Ask the conversation's backend for the next reply (compacting first when needed).
    fn start_stream(&mut self, c: Conversation, mode: StreamMode) {
        let id = c.id;
        let profile_id = c
            .profile_id
            .clone()
            .unwrap_or_else(|| self.config.active_profile.clone());
        let generation = self.next_gen;
        self.next_gen += 1;
        let profile = self.config.profile(&profile_id).cloned();
        let turn_ws = self
            .turns
            .get(&id)
            .map(|t| t.workspace.clone())
            .unwrap_or_else(|| self.workspace.clone());
        let mut request = profile.as_ref().map(|p| build_request(&c, p, &self.config));
        if let (Some(req), Some(ag)) = (request.as_mut(), self.agent_for(&c))
            && turn_ws.root.is_some()
        {
            let ov = self.existing_overlay(&turn_ws, id);
            req.system = agent::system_prompt(
                &self.config,
                &ag,
                &turn_ws,
                ov.as_ref(),
                &self.sandbox_status,
            );
            let names: Vec<String> = ag.tools.iter().filter(|t| ag.allows(t)).cloned().collect();
            req.tools = self.registry.specs(&names);
        }
        let split = match (&profile, mode) {
            (_, StreamMode::CompactOnly) => compaction_split(&c),
            (Some(p), StreamMode::Reply) if needs_compaction(&c, p) => compaction_split(&c),
            _ => None,
        };
        if mode == StreamMode::CompactOnly && split.is_none() {
            self.emit(Event::Info("There's nothing to compact yet.".into()));
            return;
        }
        let cfg = self.config.clone();
        let factory = self.factory.clone();
        let tx = self.tx.clone();
        let task = tokio::spawn(async move {
            let send = |item| {
                let _ = tx.send(Msg::Stream {
                    conversation: id,
                    generation,
                    item,
                });
            };
            let (Some(profile), Some(mut request)) = (profile, request) else {
                send(StreamItem::Failed(format!(
                    "There's no backend profile called \"{profile_id}\"."
                )));
                return;
            };
            let key = match resolve_api_key(&profile.api_key).await {
                Ok(k) => k,
                Err(e) => {
                    send(StreamItem::Failed(format!(
                        "Couldn't get the API key for {}: {e}",
                        profile.name
                    )));
                    return;
                }
            };
            let backend = factory(&profile, key);
            if let Some(split) = split {
                let mut earlier = c.clone();
                earlier.messages.truncate(split);
                let mut sreq = build_request(&earlier, &profile, &cfg);
                sreq.system =
                    "You write concise summaries of conversations so they can be continued later."
                        .into();
                sreq.tools = Vec::new();
                sreq.max_output = profile.max_output.min(4096);
                sreq.messages.push(Message::user(
                    "Summarise the conversation so far for yourself: goals, decisions, files touched, \
                     current state and open threads. Be concise but keep every detail needed to continue.",
                ));
                let mut text = String::new();
                let mut stream = backend.stream(sreq);
                while let Some(item) = stream.next().await {
                    match item {
                        Ok(StreamEvent::TextDelta(t)) => text.push_str(&t),
                        Ok(_) => {}
                        Err(e) => {
                            send(StreamItem::Failed(format!(
                                "Compacting failed: {}",
                                error_text(&e)
                            )));
                            return;
                        }
                    }
                }
                send(StreamItem::Summary {
                    split,
                    text: text.clone(),
                });
                if mode == StreamMode::CompactOnly {
                    send(StreamItem::CompactOnlyDone);
                    return;
                }
                let mut compacted = c.clone();
                compacted.messages.insert(
                    split,
                    Message::new(Role::Assistant, vec![Part::Summary(text)]),
                );
                let rebuilt = build_request(&compacted, &profile, &cfg);
                request.messages = rebuilt.messages;
            }
            let mut stream = backend.stream(request);
            while let Some(item) = stream.next().await {
                match item {
                    Ok(ev) => {
                        let done = matches!(ev, StreamEvent::Done(_));
                        send(StreamItem::Event(ev));
                        if done {
                            return;
                        }
                    }
                    Err(e) => {
                        send(StreamItem::Failed(error_text(&e)));
                        return;
                    }
                }
            }
            send(StreamItem::Ended);
        });
        self.streams.insert(
            id,
            Streaming {
                generation,
                abort: task.abort_handle(),
                message: Message::new(Role::Assistant, Vec::new()),
                first_delta: None,
            },
        );
        if mode == StreamMode::Reply {
            self.emit(Event::StreamStarted(id));
        }
    }

    fn stream_item(&mut self, id: ConversationId, generation: u64, item: StreamItem) {
        let Some(s) = self.streams.get_mut(&id) else {
            return;
        };
        if s.generation != generation {
            return;
        }
        match item {
            StreamItem::Event(StreamEvent::TextDelta(t)) => {
                s.first_delta.get_or_insert_with(Instant::now);
                push_text(&mut s.message.parts, &t, false);
                self.emit(Event::Delta {
                    conversation: id,
                    kind: DeltaKind::Text,
                    text: t,
                });
            }
            StreamItem::Event(StreamEvent::ReasoningDelta(t)) => {
                s.first_delta.get_or_insert_with(Instant::now);
                push_text(&mut s.message.parts, &t, true);
                self.emit(Event::Delta {
                    conversation: id,
                    kind: DeltaKind::Reasoning,
                    text: t,
                });
            }
            StreamItem::Event(StreamEvent::ToolCall {
                id: call_id,
                name,
                arguments,
            }) => s.message.parts.push(Part::ToolCall {
                id: call_id,
                name,
                arguments,
            }),
            StreamItem::Event(StreamEvent::Usage(u)) => s.message.usage = Some(u),
            StreamItem::Event(StreamEvent::Done(_)) | StreamItem::Ended => self.finish(id, false),
            StreamItem::Failed(error) => self.fail(id, error),
            StreamItem::Summary { split, text } => {
                if let Some(c) = self.convs.get_mut(&id) {
                    let at = split.min(c.messages.len());
                    c.messages
                        .insert(at, Message::new(Role::Assistant, vec![Part::Summary(text)]));
                    c.updated = Utc::now();
                    let c = c.clone();
                    self.save(&c);
                    self.emit(Event::Conversation(c));
                }
                self.emit(Event::Compacted(id));
            }
            StreamItem::CompactOnlyDone => {
                self.streams.remove(&id);
            }
        }
    }

    /// Store the (possibly partial) reply and report it.
    fn finish(&mut self, id: ConversationId, stopped: bool) {
        let Some(mut s) = self.streams.remove(&id) else {
            return;
        };
        s.message.stopped = stopped;
        let tokens_per_sec = s.first_delta.and_then(|t| {
            let secs = t.elapsed().as_secs_f32();
            let tokens = s
                .message
                .usage
                .map(|u| u.completion_tokens)
                .unwrap_or_else(|| {
                    estimate_tokens(&s.message.text()) + estimate_tokens(&s.message.reasoning())
                });
            (secs > 0.001 && tokens > 0).then(|| tokens as f32 / secs)
        });
        let message = s.message;
        self.append(id, message.clone());
        let calls: Vec<(String, String, String)> = message
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::ToolCall {
                    id,
                    name,
                    arguments,
                } => Some((id.clone(), name.clone(), arguments.clone())),
                _ => None,
            })
            .collect();
        self.emit(Event::StreamFinished {
            conversation: id,
            message,
            tokens_per_sec,
        });
        self.emit_list();
        if stopped || calls.is_empty() {
            self.end_turn(id);
            return;
        }
        match self.turns.get(&id).map(|t| t.round) {
            Some(round) if round >= self.options.step_limit => {
                self.emit(Event::Error(format!(
                    "Stopped after {} tool rounds (the step limit).",
                    self.options.step_limit
                )));
                self.end_turn(id);
            }
            Some(_) => self.spawn_tools(id, calls),
            None => {}
        }
    }

    fn fail(&mut self, id: ConversationId, error: String) {
        let Some(mut s) = self.streams.remove(&id) else {
            return;
        };
        s.message.error = Some(error.clone());
        if let Some(c) = self.append(id, s.message) {
            self.emit(Event::Conversation(c));
        }
        self.emit(Event::StreamFailed {
            conversation: id,
            error,
        });
        self.end_turn(id);
    }

    fn append(&mut self, id: ConversationId, message: Message) -> Option<Conversation> {
        let current = self.workspace.id.clone();
        let c = self.convs.get_mut(&id)?;
        c.messages.push(message);
        c.updated = Utc::now();
        let c = c.clone();
        self.save(&c);
        if c.workspace_id != current {
            self.convs.remove(&id);
        }
        Some(c)
    }
}

fn push_text(parts: &mut Vec<Part>, t: &str, reasoning: bool) {
    match (parts.last_mut(), reasoning) {
        (Some(Part::Text(s)), false) | (Some(Part::Reasoning(s)), true) => s.push_str(t),
        _ => parts.push(if reasoning {
            Part::Reasoning(t.to_string())
        } else {
            Part::Text(t.to_string())
        }),
    }
}

fn error_text(e: &BackendError) -> String {
    let mut s = e.to_string();
    if let Some(first) = s.get(..1) {
        s.replace_range(..1, &first.to_uppercase());
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{
        Backend, BackendError, ChatRequest, EventStream, StopReason, StreamEvent,
    };
    use crate::config::{ApiKeySource, BackendKind, Config, Profile, save_config};
    use crate::model::{Usage, Workspace};
    use futures_util::StreamExt;
    use futures_util::future::BoxFuture;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    struct Fake {
        script: Vec<(u64, StreamEvent)>,
    }

    impl Backend for Fake {
        fn stream(&self, _req: ChatRequest) -> EventStream {
            futures_util::stream::iter(self.script.clone())
                .then(|(ms, ev)| async move {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    Ok::<_, BackendError>(ev)
                })
                .boxed()
        }
        fn list_models(&self) -> BoxFuture<'static, Result<Vec<String>, BackendError>> {
            Box::pin(async { Ok(vec!["fake-model".to_string()]) })
        }
    }

    fn reply(text: &[&str], delay_ms: u64) -> Vec<(u64, StreamEvent)> {
        let mut s: Vec<_> = text
            .iter()
            .map(|t| (delay_ms, StreamEvent::TextDelta(t.to_string())))
            .collect();
        s.push((
            0,
            StreamEvent::Usage(Usage {
                prompt_tokens: 10,
                completion_tokens: 3,
            }),
        ));
        s.push((0, StreamEvent::Done(StopReason::EndTurn)));
        s
    }

    fn profile(id: &str, base_url: &str) -> Profile {
        Profile {
            id: id.into(),
            name: id.into(),
            kind: BackendKind::OpenAi,
            base_url: base_url.into(),
            api_key: ApiKeySource::None,
            model: "m".into(),
            context_limit: 1000,
            max_output: 100,
            extra_body: Default::default(),
        }
    }

    struct Rig {
        _dir: tempfile::TempDir,
        root: std::path::PathBuf,
        store: Store,
        handle: EngineHandle,
        notified: Arc<AtomicUsize>,
        received: usize,
    }

    fn closed_port() -> String {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let a = l.local_addr().unwrap();
        drop(l);
        format!("http://{a}/v1")
    }

    /// Profiles: "fast" (instant reply), "slow" (3 deltas then a 10 s pause), "dead" (real
    /// OpenAI backend on a closed port).
    fn rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let cfg = Config {
            active_profile: "fast".into(),
            profiles: vec![
                profile("fast", "http://fake"),
                profile("slow", "http://fake"),
                profile("dead", &closed_port()),
            ],
            ..Config::default()
        };
        let cfg_path = root.join("config/config.toml");
        save_config(&cfg_path, &cfg).unwrap();
        let store = Store::new(root.join("data"));
        let notified = Arc::new(AtomicUsize::new(0));
        let n = notified.clone();
        let notify: Notify = Arc::new(move || {
            n.fetch_add(1, Ordering::SeqCst);
        });
        let factory: BackendFactory =
            Arc::new(|p: &Profile, key: Option<String>| match p.id.as_str() {
                "fast" => Arc::new(Fake {
                    script: reply(&["Hel", "lo"], 5),
                }) as Arc<dyn Backend>,
                "slow" => {
                    let mut s = reply(&["a", "b", "c"], 20);
                    s.insert(3, (10_000, StreamEvent::TextDelta("never".into())));
                    Arc::new(Fake { script: s })
                }
                _ => crate::backend::make_backend(p, key),
            });
        let handle = Engine::start_with(cfg_path, store.clone(), notify, factory);
        Rig {
            _dir: dir,
            root,
            store,
            handle,
            notified,
            received: 0,
        }
    }

    impl Rig {
        fn next(&mut self) -> Event {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(e) = self.handle.try_recv() {
                    self.received += 1;
                    return e;
                }
                assert!(Instant::now() < deadline, "timed out waiting for an event");
                std::thread::sleep(Duration::from_millis(2));
            }
        }

        /// Skip events until one matches.
        fn until(&mut self, mut pred: impl FnMut(&Event) -> bool) -> Event {
            loop {
                let e = self.next();
                if pred(&e) {
                    return e;
                }
            }
        }

        fn started(&mut self) {
            self.until(|e| matches!(e, Event::WorkspaceOpened { .. }));
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

        fn profile(&self, c: ConversationId, p: &str) {
            self.handle
                .send(Command::SetConversationProfile(c, Some(p.into())));
        }
    }

    #[test]
    fn engine_starts_with_config_and_scratch() {
        let mut r = rig();
        match r.next() {
            Event::ConfigLoaded { config, warning } => {
                assert_eq!(config.active_profile, "fast");
                assert!(warning.is_none());
            }
            e => panic!("unexpected {e:?}"),
        }
        match r.next() {
            Event::WorkspaceOpened {
                workspace,
                conversations,
                ..
            } => {
                assert_eq!(workspace, Workspace::scratch());
                assert!(conversations.is_empty());
            }
            e => panic!("unexpected {e:?}"),
        }
        assert!(r.notified.load(Ordering::SeqCst) >= r.received);
    }

    #[test]
    fn engine_streams_and_saves() {
        let mut r = rig();
        r.started();
        let c = r.new_conversation();
        r.send(c, "hi there\nsecond line");
        r.until(|e| matches!(e, Event::StreamStarted(id) if *id == c));
        let mut text = String::new();
        let finished = loop {
            match r.next() {
                Event::Delta {
                    conversation,
                    kind: DeltaKind::Text,
                    text: t,
                } => {
                    assert_eq!(conversation, c);
                    text.push_str(&t);
                }
                Event::StreamFinished {
                    conversation,
                    message,
                    tokens_per_sec,
                } => {
                    assert_eq!(conversation, c);
                    assert!(tokens_per_sec.is_some());
                    break message;
                }
                _ => {}
            }
        };
        assert_eq!(text, "Hello");
        assert_eq!(finished.text(), "Hello");
        assert_eq!(
            finished.usage,
            Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 3
            })
        );
        assert!(!finished.stopped);
        let saved = r.store.load(&Workspace::scratch().id, &c).unwrap();
        assert_eq!(saved.messages.len(), 2);
        assert_eq!(saved.title, "hi there");
        assert!(r.notified.load(Ordering::SeqCst) >= r.received);
    }

    #[test]
    fn long_first_line_titles_are_cut_to_40_chars() {
        let mut r = rig();
        r.started();
        let c = r.new_conversation();
        r.send(c, &"x".repeat(60));
        r.until(|e| matches!(e, Event::StreamFinished { .. }));
        let saved = r.store.load(&Workspace::scratch().id, &c).unwrap();
        assert_eq!(saved.title.chars().count(), 40);
    }

    #[test]
    fn stop_keeps_partial_reply() {
        let mut r = rig();
        r.started();
        let c = r.new_conversation();
        r.profile(c, "slow");
        r.send(c, "go");
        let mut deltas = 0;
        r.until(|e| {
            if matches!(e, Event::Delta { .. }) {
                deltas += 1;
            }
            deltas == 2
        });
        let t = Instant::now();
        r.handle.send(Command::Stop(c));
        let msg = match r.until(|e| matches!(e, Event::StreamFinished { .. })) {
            Event::StreamFinished { message, .. } => message,
            _ => unreachable!(),
        };
        assert!(t.elapsed() < Duration::from_secs(1));
        assert!(msg.stopped);
        assert!(msg.text().starts_with("ab"), "got {:?}", msg.text());
        assert!(!msg.text().contains("never"));
        let saved = r.store.load(&Workspace::scratch().id, &c).unwrap();
        assert!(saved.messages.last().unwrap().stopped);
    }

    #[test]
    fn engine_reports_connection_refused() {
        let mut r = rig();
        r.started();
        let c = r.new_conversation();
        r.profile(c, "dead");
        r.send(c, "hello?");
        match r.until(|e| matches!(e, Event::StreamFailed { .. })) {
            Event::StreamFailed {
                conversation,
                error,
            } => {
                assert_eq!(conversation, c);
                assert!(
                    error.to_lowercase().contains("couldn't reach"),
                    "got {error}"
                );
                assert!(error.contains("Connection refused"), "got {error}");
            }
            _ => unreachable!(),
        }
        let saved = r.store.load(&Workspace::scratch().id, &c).unwrap();
        assert!(saved.messages.last().unwrap().error.is_some());
        r.profile(c, "fast");
        r.send(c, "now?");
        match r.until(|e| matches!(e, Event::StreamFinished { .. } | Event::Error(_))) {
            Event::StreamFinished { message, .. } => assert_eq!(message.text(), "Hello"),
            e => panic!("unexpected {e:?}"),
        }
    }

    #[test]
    fn retry_streams_without_adding_a_user_message() {
        let mut r = rig();
        r.started();
        let c = r.new_conversation();
        r.profile(c, "dead");
        r.send(c, "hello?");
        r.until(|e| matches!(e, Event::StreamFailed { .. }));
        r.profile(c, "fast");
        r.handle.send(Command::Retry(c));
        match r.until(|e| matches!(e, Event::StreamFinished { .. } | Event::Error(_))) {
            Event::StreamFinished { message, .. } => assert_eq!(message.text(), "Hello"),
            e => panic!("unexpected {e:?}"),
        }
        let saved = r.store.load(&Workspace::scratch().id, &c).unwrap();
        let users = saved
            .messages
            .iter()
            .filter(|m| m.role == crate::model::Role::User)
            .count();
        assert_eq!(users, 1);
        // The failed reply is replaced by the new one.
        assert_eq!(saved.messages.len(), 2);
        assert!(saved.messages[1].error.is_none());
    }

    #[test]
    fn delete_while_streaming_reports_deletion() {
        let mut r = rig();
        r.started();
        let c = r.new_conversation();
        r.profile(c, "slow");
        r.send(c, "go");
        r.until(|e| matches!(e, Event::Delta { .. }));
        r.handle.send(Command::DeleteConversation(c));
        match r.until(|e| {
            matches!(
                e,
                Event::ConversationDeleted(_) | Event::StreamFinished { .. }
            )
        }) {
            Event::ConversationDeleted(id) => assert_eq!(id, c),
            e => panic!("unexpected {e:?}"),
        }
        assert!(r.store.load(&Workspace::scratch().id, &c).is_err());
    }

    #[test]
    fn retry_after_stop_replaces_the_partial_reply() {
        let mut r = rig();
        r.started();
        let c = r.new_conversation();
        r.profile(c, "slow");
        r.send(c, "go");
        let mut deltas = 0;
        r.until(|e| {
            if matches!(e, Event::Delta { .. }) {
                deltas += 1;
            }
            deltas == 2
        });
        r.handle.send(Command::Stop(c));
        r.until(|e| matches!(e, Event::StreamFinished { .. }));
        r.profile(c, "fast");
        r.handle.send(Command::Retry(c));
        r.until(
            |e| matches!(e, Event::StreamFinished { message, .. } if message.text() == "Hello"),
        );
        let saved = r.store.load(&Workspace::scratch().id, &c).unwrap();
        let texts: Vec<_> = saved.messages.iter().map(|m| m.text()).collect();
        assert_eq!(texts, ["go", "Hello"]);
    }

    #[test]
    fn new_conversation_is_reported_as_created() {
        let mut r = rig();
        r.started();
        r.handle.send(Command::NewConversation);
        let id = match r
            .until(|e| matches!(e, Event::ConversationCreated(_) | Event::Conversation(_)))
        {
            Event::ConversationCreated(c) => c.id,
            e => panic!("unexpected {e:?}"),
        };
        // Changing it later is a plain update, not a creation.
        r.handle.send(Command::RenameConversation(id, "x".into()));
        assert!(matches!(
            r.until(|e| matches!(e, Event::ConversationCreated(_) | Event::Conversation(_))),
            Event::Conversation(_)
        ));
    }

    #[test]
    fn dropping_the_handle_keeps_partial_replies() {
        let mut r = rig();
        r.started();
        let c = r.new_conversation();
        r.profile(c, "slow");
        r.send(c, "go");
        let mut deltas = 0;
        r.until(|e| {
            if matches!(e, Event::Delta { .. }) {
                deltas += 1;
            }
            deltas == 2
        });
        let Rig {
            handle,
            store,
            _dir,
            ..
        } = r;
        drop(handle);
        let saved = store.load(&Workspace::scratch().id, &c).unwrap();
        let last = saved.messages.last().unwrap();
        assert!(last.stopped, "partial reply saved as stopped");
        assert!(last.text().starts_with("ab"));
    }

    #[test]
    fn missing_profile_fails_the_stream() {
        let mut r = rig();
        r.started();
        let c = r.new_conversation();
        r.profile(c, "nope");
        r.send(c, "x");
        match r.until(|e| matches!(e, Event::StreamFailed { .. })) {
            Event::StreamFailed { error, .. } => assert!(error.contains("nope")),
            _ => unreachable!(),
        }
    }

    #[test]
    fn two_conversations_stream_concurrently() {
        let mut r = rig();
        r.started();
        let a = r.new_conversation();
        let b = r.new_conversation();
        r.profile(a, "slow");
        r.profile(b, "slow");
        r.send(a, "1");
        r.send(b, "2");
        let mut started = Vec::new();
        while started.len() < 2 {
            match r.next() {
                Event::StreamStarted(id) => started.push(id),
                Event::StreamFinished { .. } => panic!("finished before both started"),
                _ => {}
            }
        }
        assert!(started.contains(&a) && started.contains(&b));
        r.handle.send(Command::Stop(a));
        r.handle.send(Command::Stop(b));
    }

    #[test]
    fn send_while_streaming_is_rejected() {
        let mut r = rig();
        r.started();
        let c = r.new_conversation();
        r.profile(c, "slow");
        r.send(c, "1");
        r.until(|e| matches!(e, Event::StreamStarted(_)));
        r.send(c, "2");
        match r.until(|e| matches!(e, Event::Error(_))) {
            Event::Error(m) => assert!(m.contains("already replying"), "got {m}"),
            _ => unreachable!(),
        }
        r.handle.send(Command::Stop(c));
    }

    #[test]
    fn workspace_switch_lists_its_conversations() {
        let mut r = rig();
        r.started();
        r.new_conversation();
        let proj = r.root.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        r.handle.send(Command::OpenWorkspace(Some(proj.clone())));
        match r.until(|e| matches!(e, Event::WorkspaceOpened { .. })) {
            Event::WorkspaceOpened {
                workspace,
                conversations,
                recent,
                ..
            } => {
                assert_eq!(workspace.name, "proj");
                assert!(conversations.is_empty());
                assert_eq!(recent[0].workspace, workspace);
            }
            _ => unreachable!(),
        }
        r.handle.send(Command::OpenWorkspace(None));
        match r.until(|e| matches!(e, Event::WorkspaceOpened { .. })) {
            Event::WorkspaceOpened {
                workspace,
                conversations,
                ..
            } => {
                assert_eq!(workspace, Workspace::scratch());
                assert_eq!(conversations.len(), 1);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn opening_a_missing_folder_reports_an_error() {
        let mut r = rig();
        r.started();
        r.handle
            .send(Command::OpenWorkspace(Some(r.root.join("missing"))));
        assert!(matches!(
            r.until(|e| matches!(e, Event::Error(_) | Event::WorkspaceOpened { .. })),
            Event::Error(_)
        ));
    }

    #[test]
    fn rename_and_delete_update_the_list() {
        let mut r = rig();
        r.started();
        let c = r.new_conversation();
        r.handle
            .send(Command::RenameConversation(c, "Renamed".into()));
        match r.until(
            |e| matches!(e, Event::ConversationList(l) if l.iter().any(|m| m.title == "Renamed")),
        ) {
            Event::ConversationList(l) => assert_eq!(l.len(), 1),
            _ => unreachable!(),
        }
        r.handle.send(Command::DeleteConversation(c));
        r.until(|e| matches!(e, Event::ConversationList(l) if l.is_empty()));
        assert!(r.store.load(&Workspace::scratch().id, &c).is_err());
    }

    #[test]
    fn test_profile_lists_models() {
        let mut r = rig();
        r.started();
        r.handle
            .send(Command::TestProfile(profile("fast", "http://fake")));
        match r.until(|e| matches!(e, Event::ProfileTested { .. })) {
            Event::ProfileTested { profile_id, result } => {
                assert_eq!(profile_id, "fast");
                assert_eq!(result.unwrap(), vec!["fake-model"]);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn save_config_persists() {
        let mut r = rig();
        r.started();
        let cfg = Config {
            font_size: 20.0,
            active_profile: "slow".into(),
            ..Config::default()
        };
        r.handle.send(Command::SaveConfig(cfg.clone()));
        r.until(|e| matches!(e, Event::ConfigSaved));
        let (back, _) = crate::config::load_config(&r.root.join("config/config.toml"));
        assert_eq!(back, cfg);
    }

    #[test]
    fn layout_is_saved_through_the_handle() {
        let mut r = rig();
        r.started();
        r.handle
            .save_layout(&Workspace::scratch().id, "{\"a\":1}".into());
        let deadline = Instant::now() + Duration::from_secs(2);
        while r.store.load_layout(&Workspace::scratch().id).is_none() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    // ---- agent loop ----------------------------------------------------------------

    use crate::overlay::{ChangeKind, Overlay};
    use crate::permissions::{Action, Decision, Permissions, Rule};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Replies with the next scripted stream on each call (the last one repeats) and records
    /// every request.
    struct Seq {
        scripts: Mutex<VecDeque<Vec<StreamEvent>>>,
        requests: Arc<Mutex<Vec<ChatRequest>>>,
    }

    impl Backend for Seq {
        fn stream(&self, req: ChatRequest) -> EventStream {
            self.requests.lock().unwrap().push(req);
            let mut q = self.scripts.lock().unwrap();
            let script = if q.len() > 1 {
                q.pop_front().unwrap()
            } else {
                q.front().cloned().unwrap_or_default()
            };
            futures_util::stream::iter(script.into_iter().map(Ok)).boxed()
        }
        fn list_models(&self) -> BoxFuture<'static, Result<Vec<String>, BackendError>> {
            Box::pin(async { Ok(vec![]) })
        }
    }

    fn call(id: &str, name: &str, args: serde_json::Value) -> Vec<StreamEvent> {
        vec![
            StreamEvent::ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: args.to_string(),
            },
            StreamEvent::Done(StopReason::ToolUse),
        ]
    }

    fn say(text: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::TextDelta(text.into()),
            StreamEvent::Done(StopReason::EndTurn),
        ]
    }

    struct Agent {
        r: Rig,
        ws: std::path::PathBuf,
        conv: ConversationId,
        requests: Arc<Mutex<Vec<ChatRequest>>>,
    }

    fn agent_rig(
        scripts: Vec<Vec<StreamEvent>>,
        step_limit: u32,
        rules: Option<Vec<Rule>>,
        context_limit: u32,
    ) -> Agent {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let ws = root.join("project");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("a.txt"), "pineapple\n").unwrap();
        let mut p = profile("agent", "http://fake");
        p.context_limit = context_limit;
        let cfg = Config {
            active_profile: "agent".into(),
            profiles: vec![p],
            ..Config::default()
        };
        let cfg_path = root.join("config/config.toml");
        save_config(&cfg_path, &cfg).unwrap();
        let store = Store::new(root.join("data"));
        if let Some(rules) = rules {
            let w = store.workspace_for(&ws).unwrap();
            Permissions { rules }
                .save(&store.permissions_path(&w.id))
                .unwrap();
        }
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seq = Arc::new(Seq {
            scripts: Mutex::new(scripts.into()),
            requests: requests.clone(),
        });
        let factory: BackendFactory =
            Arc::new(move |_p: &Profile, _k: Option<String>| seq.clone() as Arc<dyn Backend>);
        let notified = Arc::new(AtomicUsize::new(0));
        let n = notified.clone();
        let notify: Notify = Arc::new(move || {
            n.fetch_add(1, Ordering::SeqCst);
        });
        let handle = Engine::start_with_options(
            cfg_path,
            store.clone(),
            notify,
            factory,
            EngineOptions {
                step_limit,
                sandbox: false,
            },
        );
        let mut r = Rig {
            _dir: dir,
            root,
            store,
            handle,
            notified,
            received: 0,
        };
        r.started();
        r.handle.send(Command::OpenWorkspace(Some(ws.clone())));
        r.until(
            |e| matches!(e, Event::WorkspaceOpened { workspace, .. } if workspace.root.is_some()),
        );
        let conv = r.new_conversation();
        Agent {
            r,
            ws,
            conv,
            requests,
        }
    }

    impl Agent {
        fn overlay(&self) -> Overlay {
            let w = self.r.store.workspace_for(&self.ws).unwrap();
            Overlay::open(&self.r.store.overlay_dir(&w.id, &self.conv), &self.ws).unwrap()
        }
        fn saved(&self) -> Conversation {
            let w = self.r.store.workspace_for(&self.ws).unwrap();
            self.r.store.load(&w.id, &self.conv).unwrap()
        }
        fn finish_with(&mut self, text: &str) {
            let t = text.to_string();
            self.r.until(
                |e| matches!(e, Event::StreamFinished { message, .. } if message.text() == t),
            );
        }
    }

    fn defaults_with(extra: Rule) -> Vec<Rule> {
        let mut r = Permissions::defaults().rules;
        r.push(extra);
        r
    }

    #[test]
    fn tool_round_trip_reads_file() {
        let mut a = agent_rig(
            vec![
                call("c1", "read", serde_json::json!({"path": "a.txt"})),
                say("done"),
            ],
            50,
            None,
            1000,
        );
        a.r.send(a.conv, "read it");
        a.r.until(|e| matches!(e, Event::ToolStarted { name, .. } if name == "read"));
        match a.r.until(|e| matches!(e, Event::ToolFinished { .. })) {
            Event::ToolFinished { output, .. } => {
                assert!(output.content.contains("pineapple"), "{output:?}")
            }
            _ => unreachable!(),
        }
        a.finish_with("done");
        let roles: Vec<Role> = a.saved().messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            [Role::User, Role::Assistant, Role::Tool, Role::Assistant]
        );
        let reqs = a.requests.lock().unwrap();
        assert_eq!(reqs.len(), 2);
        assert!(reqs[0].tools.iter().any(|t| t.name == "bash"));
        assert!(reqs[0].system.contains(&a.ws.display().to_string()));
        assert!(reqs[1].messages.iter().any(|m| m.role == Role::Tool));
    }

    #[test]
    fn edit_lands_in_overlay_and_changes_event() {
        let mut a = agent_rig(
            vec![
                call(
                    "c1",
                    "edit",
                    serde_json::json!({"path": "a.txt", "old_string": "pineapple", "new_string": "mango"}),
                ),
                say("edited"),
            ],
            50,
            None,
            1000,
        );
        a.r.send(a.conv, "change it");
        match a
            .r
            .until(|e| matches!(e, Event::Changes { changes, .. } if !changes.is_empty()))
        {
            Event::Changes { changes, .. } => {
                assert_eq!(changes[0].path, std::path::PathBuf::from("a.txt"));
                assert_eq!(changes[0].kind, ChangeKind::Modified);
            }
            _ => unreachable!(),
        }
        a.finish_with("edited");
        assert_eq!(
            std::fs::read_to_string(a.ws.join("a.txt")).unwrap(),
            "pineapple\n"
        );
        assert_eq!(
            a.overlay().read(std::path::Path::new("a.txt")).unwrap(),
            b"mango\n"
        );
    }

    #[test]
    fn ask_waits_for_approval() {
        let rules = defaults_with(Rule {
            tool: "write".into(),
            prefix: None,
            action: Action::Ask,
        });
        let mut a = agent_rig(
            vec![
                call(
                    "c1",
                    "write",
                    serde_json::json!({"path": "b.txt", "content": "B"}),
                ),
                say("ok"),
            ],
            50,
            Some(rules),
            1000,
        );
        a.r.send(a.conv, "write");
        let id = match a.r.until(|e| matches!(e, Event::ApprovalNeeded(_))) {
            Event::ApprovalNeeded(req) => {
                assert_eq!(req.tool, "write");
                assert_eq!(req.conversation, a.conv);
                req.id
            }
            _ => unreachable!(),
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            a.overlay().read(std::path::Path::new("b.txt")).is_err(),
            "nothing runs before approval"
        );
        a.r.handle.send(Command::Approve {
            id,
            decision: Decision::Once,
        });
        a.r.until(|e| matches!(e, Event::ApprovalResolved { .. }));
        a.finish_with("ok");
        assert_eq!(
            a.overlay().read(std::path::Path::new("b.txt")).unwrap(),
            b"B"
        );
    }

    #[test]
    fn deny_reports_to_model() {
        let rules = defaults_with(Rule {
            tool: "write".into(),
            prefix: None,
            action: Action::Ask,
        });
        let mut a = agent_rig(
            vec![
                call(
                    "c1",
                    "write",
                    serde_json::json!({"path": "b.txt", "content": "B"}),
                ),
                say("ok"),
            ],
            50,
            Some(rules),
            1000,
        );
        a.r.send(a.conv, "write");
        let id = match a.r.until(|e| matches!(e, Event::ApprovalNeeded(_))) {
            Event::ApprovalNeeded(req) => req.id,
            _ => unreachable!(),
        };
        a.r.handle.send(Command::Approve {
            id,
            decision: Decision::Deny,
        });
        match a.r.until(|e| matches!(e, Event::ToolFinished { .. })) {
            Event::ToolFinished { output, .. } => {
                assert!(output.is_error);
                assert!(output.content.contains("denied"));
            }
            _ => unreachable!(),
        }
        a.finish_with("ok");
        assert!(a.overlay().read(std::path::Path::new("b.txt")).is_err());
    }

    #[test]
    fn always_for_workspace_saves_rule() {
        let rules = defaults_with(Rule {
            tool: "write".into(),
            prefix: None,
            action: Action::Ask,
        });
        let mut a = agent_rig(
            vec![
                call(
                    "c1",
                    "write",
                    serde_json::json!({"path": "b.txt", "content": "B"}),
                ),
                say("ok"),
            ],
            50,
            Some(rules),
            1000,
        );
        a.r.send(a.conv, "write");
        let id = match a.r.until(|e| matches!(e, Event::ApprovalNeeded(_))) {
            Event::ApprovalNeeded(req) => req.id,
            _ => unreachable!(),
        };
        a.r.handle.send(Command::Approve {
            id,
            decision: Decision::AlwaysForWorkspace(String::new()),
        });
        a.finish_with("ok");
        let w = a.r.store.workspace_for(&a.ws).unwrap();
        let (perms, _) = Permissions::load(&a.r.store.permissions_path(&w.id));
        assert_eq!(
            perms.decide("write", &serde_json::json!({"path": "x"})),
            Action::Allow
        );
    }

    #[test]
    fn step_limit_stops_loop() {
        let mut a = agent_rig(
            vec![call("c1", "list", serde_json::json!({}))],
            3,
            None,
            1000,
        );
        a.r.send(a.conv, "loop forever");
        match a
            .r
            .until(|e| matches!(e, Event::Error(m) if m.contains("step limit")))
        {
            Event::Error(m) => assert!(m.contains('3')),
            _ => unreachable!(),
        }
        std::thread::sleep(Duration::from_millis(100));
        let tools = a
            .saved()
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .count();
        assert_eq!(tools, 3);
    }

    #[test]
    fn stop_during_approval_ends_turn() {
        let rules = defaults_with(Rule {
            tool: "write".into(),
            prefix: None,
            action: Action::Ask,
        });
        let mut a = agent_rig(
            vec![
                call(
                    "c1",
                    "write",
                    serde_json::json!({"path": "b.txt", "content": "B"}),
                ),
                say("after"),
            ],
            50,
            Some(rules),
            1000,
        );
        a.r.send(a.conv, "write");
        a.r.until(|e| matches!(e, Event::ApprovalNeeded(_)));
        a.r.handle.send(Command::Stop(a.conv));
        a.r.until(|e| matches!(e, Event::ApprovalResolved { .. }));
        std::thread::sleep(Duration::from_millis(100));
        assert!(a.overlay().read(std::path::Path::new("b.txt")).is_err());
        // The conversation can carry on; the cut-off call gets a placeholder result.
        a.r.send(a.conv, "carry on");
        a.finish_with("after");
        let reqs = a.requests.lock().unwrap();
        let last = reqs.last().unwrap();
        assert!(last.messages.iter().flat_map(|m| m.parts.iter()).any(|p| matches!(p, Part::ToolResult { content, .. } if content.contains("not run") || content.contains("denied"))));
    }

    #[test]
    fn retry_restores_checkpoint() {
        let mut a = agent_rig(
            vec![
                call(
                    "c1",
                    "write",
                    serde_json::json!({"path": "b.txt", "content": "B"}),
                ),
                say("wrote"),
                say("again"),
            ],
            50,
            None,
            1000,
        );
        a.r.send(a.conv, "write b");
        a.finish_with("wrote");
        assert!(a.overlay().read(std::path::Path::new("b.txt")).is_ok());
        a.r.handle.send(Command::Retry(a.conv));
        a.finish_with("again");
        assert!(
            a.overlay().read(std::path::Path::new("b.txt")).is_err(),
            "retry undid the attempt's changes"
        );
        let roles: Vec<Role> = a.saved().messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, [Role::User, Role::Assistant]);
    }

    #[test]
    fn delete_conversation_discards_overlay() {
        let mut a = agent_rig(
            vec![
                call(
                    "c1",
                    "write",
                    serde_json::json!({"path": "b.txt", "content": "B"}),
                ),
                say("wrote"),
            ],
            50,
            None,
            1000,
        );
        a.r.send(a.conv, "write b");
        a.finish_with("wrote");
        let w = a.r.store.workspace_for(&a.ws).unwrap();
        let dir = a.r.store.overlay_dir(&w.id, &a.conv);
        assert!(dir.exists());
        a.r.handle.send(Command::DeleteConversation(a.conv));
        a.r.until(|e| matches!(e, Event::ConversationDeleted(_)));
        assert!(!dir.exists());
        assert!(!a.ws.join("b.txt").exists());
    }

    #[test]
    fn scratch_has_no_tools() {
        let mut a = agent_rig(vec![say("plain")], 50, None, 1000);
        a.r.handle.send(Command::OpenWorkspace(None));
        a.r.until(
            |e| matches!(e, Event::WorkspaceOpened { workspace, .. } if workspace.root.is_none()),
        );
        let c = a.r.new_conversation();
        a.r.send(c, "hi");
        a.finish_with("plain");
        assert!(a.requests.lock().unwrap().last().unwrap().tools.is_empty());
    }

    #[test]
    fn plan_agent_cannot_write() {
        let mut a = agent_rig(
            vec![
                call(
                    "c1",
                    "write",
                    serde_json::json!({"path": "b.txt", "content": "B"}),
                ),
                say("ok"),
            ],
            50,
            None,
            1000,
        );
        a.r.handle
            .send(Command::SetConversationAgent(a.conv, Some("plan".into())));
        a.r.send(a.conv, "write");
        match a.r.until(|e| matches!(e, Event::ToolFinished { .. })) {
            Event::ToolFinished { output, .. } => assert!(
                output.is_error && output.content.contains("plan"),
                "{output:?}"
            ),
            _ => unreachable!(),
        }
        a.finish_with("ok");
        assert!(a.overlay().read(std::path::Path::new("b.txt")).is_err());
        assert!(
            !a.requests.lock().unwrap()[0]
                .tools
                .iter()
                .any(|t| t.name == "write")
        );
    }

    #[test]
    fn review_commands_accept_and_reject() {
        let mut a = agent_rig(
            vec![
                call(
                    "c1",
                    "edit",
                    serde_json::json!({"path": "a.txt", "old_string": "pineapple", "new_string": "mango"}),
                ),
                call(
                    "c2",
                    "write",
                    serde_json::json!({"path": "b.txt", "content": "B"}),
                ),
                say("done"),
            ],
            50,
            None,
            1000,
        );
        a.r.send(a.conv, "go");
        a.finish_with("done");
        a.r.handle.send(Command::AcceptFile {
            conversation: a.conv,
            path: "a.txt".into(),
            force: false,
        });
        a.r.until(|e| matches!(e, Event::Changes { changes, .. } if changes.len() == 1));
        assert_eq!(
            std::fs::read_to_string(a.ws.join("a.txt")).unwrap(),
            "mango\n"
        );
        a.r.handle.send(Command::Diff {
            conversation: a.conv,
            path: "b.txt".into(),
        });
        match a.r.until(|e| matches!(e, Event::Diff { .. })) {
            Event::Diff { hunks, .. } => assert_eq!(hunks.len(), 1),
            _ => unreachable!(),
        }
        a.r.handle.send(Command::RejectFile {
            conversation: a.conv,
            path: "b.txt".into(),
        });
        a.r.until(|e| matches!(e, Event::Changes { changes, .. } if changes.is_empty()));
        assert!(!a.ws.join("b.txt").exists());
    }

    #[test]
    fn compaction_inserts_summary() {
        let mut a = agent_rig(
            vec![say("ok"), say("SUMMARY TEXT"), say("fine")],
            50,
            None,
            200,
        );
        a.r.send(a.conv, "short");
        a.finish_with("ok");
        a.r.send(a.conv, &"y".repeat(800));
        a.r.until(|e| matches!(e, Event::Compacted(_)));
        a.finish_with("fine");
        let saved = a.saved();
        assert!(saved.messages.iter().any(|m| {
            m.parts
                .iter()
                .any(|p| matches!(p, Part::Summary(s) if s == "SUMMARY TEXT"))
        }));
        let reqs = a.requests.lock().unwrap();
        assert!(reqs[1].tools.is_empty(), "the summary request has no tools");
        let first = reqs[2].messages[0].text();
        assert!(
            first.starts_with("Summary of earlier conversation:\nSUMMARY TEXT"),
            "{first}"
        );
    }
}
