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
use crate::claude_code::permission::{HostBridge, HostMsg};
use crate::claude_code::turn::{self as cc, PermissionBridge, TurnUpdate};
use crate::config::{BackendKind, Config, Profile, load_config, resolve_api_key, save_config};
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

mod claude;
use claude::ClaudeRun;

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
    /// The user saved a chat view: write `content` into the conversation's pending changes
    /// (refused while its turn is active). `path` is workspace-relative or absolute inside it.
    SaveToOverlay {
        /// Chosen by the UI; echoed in `OverlaySaved` / `OverlaySaveFailed`.
        request: u64,
        conversation: ConversationId,
        path: PathBuf,
        content: String,
    },
    /// A `Changes` event for every conversation in this workspace that has pending changes.
    ListWorkspaceChanges,
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
        /// For a conflicted file: what changed on disk since void first changed it.
        disk: Vec<Hunk>,
    },
    ReviewError {
        conversation: ConversationId,
        path: PathBuf,
        error: String,
    },
    /// Older messages were summarised.
    Compacted(ConversationId),
    /// An agent turn started or ended (the UI shows Stop while it's active).
    TurnState {
        conversation: ConversationId,
        active: bool,
    },
    /// A `SaveToOverlay` landed; `path` is as the command gave it.
    OverlaySaved {
        request: u64,
        conversation: ConversationId,
        path: PathBuf,
    },
    /// A `SaveToOverlay` was refused or failed; nothing was written.
    OverlaySaveFailed {
        request: u64,
        conversation: ConversationId,
        path: PathBuf,
        error: String,
    },
    /// A Claude Code turn started its session: the model Claude Code is using (for the status
    /// bar; empty when it didn't say).
    ClaudeInit {
        conversation: ConversationId,
        model: String,
    },
    /// What a running turn is doing when it isn't streaming (waiting for the sandbox, Claude
    /// Code retrying the API, …). Not an error.
    TurnStatus {
        conversation: ConversationId,
        status: String,
    },
}

/// Knobs for embedding the engine (tests lower the step limit and skip the sandbox).
#[derive(Clone)]
pub struct EngineOptions {
    /// Tool rounds per turn before the agent is stopped.
    pub step_limit: u32,
    /// Set up the bash sandbox (false: bash reports it's unavailable).
    pub sandbox: bool,
    /// The home directory the sandbox (and Claude Code's state) is based on; None = `$HOME`.
    /// Tests point it at a dummy home.
    pub home: Option<PathBuf>,
    /// Where Claude Code's permission requests go; None = the built-in one.
    pub claude_bridge: Option<Arc<dyn PermissionBridge>>,
}

impl std::fmt::Debug for EngineOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineOptions")
            .field("step_limit", &self.step_limit)
            .field("sandbox", &self.sandbox)
            .field("home", &self.home)
            .field("claude_bridge", &self.claude_bridge.is_some())
            .finish()
    }
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            step_limit: 50,
            sandbox: true,
            home: None,
            claude_bridge: None,
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
        let home = options.home.clone().unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/"))
        });
        let claude_bridge = options.claude_bridge.clone().unwrap_or_else(|| {
            let tx = tx.clone();
            Arc::new(HostBridge::new(move |m| {
                tx.send(Msg::ClaudePermission(m)).is_ok()
            }))
        });
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
            home,
            claude: HashMap::new(),
            claude_bridge,
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
    /// The grace period after Stop is over: abort whatever is still running.
    ForceEnd(ConversationId),
    /// From a Claude Code turn's task.
    Claude {
        conversation: ConversationId,
        generation: u64,
        update: TurnUpdate,
    },
    /// Claude Code asks for (or withdraws) a permission prompt.
    ClaudePermission(HostMsg),
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
    /// bash is waiting for the sandbox (another chat's sandbox holds the cache lock).
    Waiting,
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
    /// Stop was pressed: let the running tool finish with its partial output, then end.
    stopping: bool,
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
    /// The home the sandbox (and Claude Code's state) is based on.
    home: PathBuf,
    claude: HashMap<ConversationId, ClaudeRun>,
    claude_bridge: Arc<dyn PermissionBridge>,
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
                    let runs: Vec<_> = self.claude.keys().copied().collect();
                    for id in runs {
                        self.stop_claude(id);
                    }
                    let ids: Vec<_> = self.streams.keys().copied().collect();
                    for id in ids {
                        if let Some(s) = self.streams.get(&id) {
                            s.abort.abort();
                        }
                        self.finish(id, true);
                    }
                    let turns: Vec<_> = self.turns.keys().copied().collect();
                    for id in turns {
                        self.finalize_turn(id);
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
                Msg::Approval { req, reply } => {
                    if !self.turns.contains_key(&req.conversation) {
                        continue;
                    }
                    self.request_approval(req, reply);
                }
                Msg::ClaudePermission(m) => self.claude_permission(m),
                Msg::Claude {
                    conversation,
                    generation,
                    update,
                } => self.claude_update(conversation, generation, update),
                Msg::ForceEnd(id) => {
                    if self.turns.get(&id).is_some_and(|t| t.stopping) {
                        self.finalize_turn(id);
                    }
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

    /// A sandbox for the current settings. A replacement keeps the old one's cache lock, so it
    /// still waits for whatever the old one has mounted.
    fn make_sandbox(&self) -> Sandbox {
        let sb = Sandbox::new(
            self.store.data_dir().to_path_buf(),
            self.home.clone(),
            self.config.sandbox.clone(),
        );
        match &self.sandbox {
            Some(old) => sb.sharing_lock_with(old),
            None => sb,
        }
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

    /// Start an agent turn on `c` (its last message is the user's). Notes for the agent
    /// (the user's rejections and chat-view edits) go with the turn's input: appended to that
    /// message and kept in the history. Returns the conversation as it now is.
    fn begin_turn(&mut self, c: &Conversation) -> Conversation {
        let ws = self.workspace.clone();
        let notes = self
            .existing_overlay(&ws, c.id)
            .map(|ov| ov.take_rejections())
            .unwrap_or_default();
        let mut c = c.clone();
        if !notes.is_empty()
            && let Some(m) = c.messages.last_mut().filter(|m| m.role == Role::User)
        {
            m.parts.push(Part::Text(format!(
                "\n\n[Notes from void]\n{}",
                notes.join("\n")
            )));
            self.convs.insert(c.id, c.clone());
            self.save(&c);
            self.emit(Event::Conversation(c.clone()));
        }
        let c = &c;
        let tools = ws.root.is_some()
            && (self.is_claude(c) || self.agent_for(c).is_some_and(|a| !a.tools.is_empty()));
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
                stopping: false,
            },
        );
        self.emit(Event::TurnState {
            conversation: c.id,
            active: true,
        });
        c.clone()
    }

    fn busy(&self, id: ConversationId) -> bool {
        self.streams.contains_key(&id) || self.turns.contains_key(&id)
    }

    /// Stop a turn: cancel its tools and refuse its pending approvals. A running tool gets a
    /// moment to finish with what it has (bash returns "stopped" plus its output); after 3 s
    /// it's aborted.
    fn end_turn(&mut self, id: ConversationId) {
        let Some(t) = self.turns.get_mut(&id) else {
            return;
        };
        t.cancel.cancel();
        let running = t.runner.is_some();
        self.withdraw_approvals(id);
        if !running {
            self.finalize_turn(id);
            return;
        }
        if let Some(t) = self.turns.get_mut(&id) {
            if t.stopping {
                return;
            }
            t.stopping = true;
        }
        let tx = self.tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            let _ = tx.send(Msg::ForceEnd(id));
        });
    }

    /// Ask the user (the GUI's approval dialog); the answer goes to `reply`. Dropping the
    /// entry (Stop, the turn's end) closes the dialog and drops `reply` unanswered.
    fn request_approval(
        &mut self,
        mut req: ApprovalRequest,
        reply: oneshot::Sender<Decision>,
    ) -> u64 {
        req.id = self.next_approval;
        self.next_approval += 1;
        self.approvals.insert(req.id, (req.conversation, reply));
        let id = req.id;
        self.emit(Event::ApprovalNeeded(req));
        id
    }

    /// Close every pending approval of the chat (their requests get no answer).
    fn withdraw_approvals(&mut self, id: ConversationId) {
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
    }

    /// End a turn now (abort a running tool) and report its changes.
    fn finalize_turn(&mut self, id: ConversationId) {
        let Some(t) = self.turns.remove(&id) else {
            return;
        };
        t.cancel.cancel();
        if let Some(r) = t.runner {
            r.abort();
        }
        self.withdraw_approvals(id);
        self.emit(Event::TurnState {
            conversation: id,
            active: false,
        });
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
            ToolMsg::Waiting => {
                let status = if self.claude.is_empty() {
                    "Waiting for the sandbox — another chat's command is using it…"
                } else {
                    "Waiting for the sandbox — a Claude Code turn is using it…"
                };
                self.emit(Event::TurnStatus {
                    conversation: id,
                    status: status.into(),
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
                t.runner = None;
                if t.stopping {
                    self.finalize_turn(id);
                    return;
                }
                t.round += 1;
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
            for (call_id, name, arguments) in calls {
                if cancel.is_cancelled() {
                    break;
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
                let output = 'out: {
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
                                kind: Default::default(),
                            };
                            let _ = tx.send(Msg::Approval { req, reply: otx });
                            match orx.await {
                                Ok(Decision::Once | Decision::AlwaysAsSuggested) => {}
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
                        on_wait: Some(Arc::new({
                            let tx = tx.clone();
                            move || {
                                let _ = tx.send(Msg::Tool(id, ToolMsg::Waiting));
                            }
                        })),
                    };
                    let out = t.run(a, ctx).await;
                    let _ = fwd.await;
                    out
                };
                send(ToolMsg::Finished { call_id, output });
            }
            send(ToolMsg::RoundDone);
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
        let review = matches!(
            cmd,
            Command::AcceptFile { .. }
                | Command::RejectFile { .. }
                | Command::AcceptHunk { .. }
                | Command::RejectHunk { .. }
                | Command::AcceptAll { .. }
                | Command::RejectAll(_)
        );
        // A running Claude Code turn has this workspace mounted under its overlay: changing
        // the real files beneath it (or its upper layer) isn't safe until it's gone.
        if review && self.claude_running_in(&self.workspace.id) {
            self.emit(Event::Error(cc::BUSY.into()));
            return;
        }
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
                self.delete_claude(id);
                self.finalize_turn(id);
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
                let scratch = self
                    .store
                    .data_dir()
                    .join("claude-scratch")
                    .join(id.to_string());
                if scratch.exists()
                    && let Err(e) = crate::fsutil::remove_tree(&scratch)
                {
                    self.emit(Event::Error(format!(
                        "Couldn't remove the chat's Claude Code folder: {e}"
                    )));
                }
                let overlay = self.store.overlay_dir(&ws, &id);
                if overlay.exists()
                    && let Err(e) = crate::fsutil::remove_tree(&overlay)
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
                if self.busy(id) {
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
                // What Claude Code saw of the replaced attempt doesn't count for what follows.
                if let Some(seen) = c.claude_upto {
                    c.claude_upto = Some(seen.min(c.messages.len()));
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
                let c = self.begin_turn(&c);
                self.reply(c, true);
            }
            Command::Stop(id) => {
                if self.stop_claude(id) {
                    return;
                }
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
                    Some(Ok(hunks)) => {
                        let ov = self.existing_overlay(&ws, conversation);
                        let conflicted = ov.as_ref().is_some_and(|o| {
                            o.changes().iter().any(|c| c.path == path && c.conflict)
                        });
                        let disk = match (conflicted, ov) {
                            (true, Some(o)) => o.disk_diff(&path).unwrap_or_default(),
                            _ => Vec::new(),
                        };
                        self.emit(Event::Diff {
                            conversation,
                            path,
                            hunks,
                            disk,
                        })
                    }
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
                if self
                    .conversation(id)
                    .cloned()
                    .is_some_and(|c| self.is_claude(&c))
                {
                    self.emit(Event::Info(
                        "Claude Code manages its own context; there's nothing to compact here."
                            .into(),
                    ));
                } else if self.streams.contains_key(&id) || self.turns.contains_key(&id) {
                    self.emit(Event::Error(
                        "Wait for the reply to finish before compacting.".into(),
                    ));
                } else if let Some(c) = self.conversation(id).cloned() {
                    self.start_stream(c, StreamMode::CompactOnly);
                }
            }
            Command::ClearSandboxCaches => {
                // A Claude Code turn may have the layers mounted (or be about to).
                if !self.claude.is_empty() {
                    self.emit(Event::Error(
                        "Can't clear the sandbox caches while a Claude Code turn is running — \
                         try again when it finishes."
                            .into(),
                    ));
                } else if let Some(sb) = &self.sandbox {
                    match sb.clear_cache_layers() {
                        Ok(true) => self.emit(Event::Info("Sandbox caches cleared.".into())),
                        Ok(false) => {
                            self.emit(Event::Info("Sandbox caches were already empty.".into()))
                        }
                        Err(e) => self.emit(Event::Error(e)),
                    }
                }
            }
            Command::SaveToOverlay {
                request,
                conversation,
                path,
                content,
            } => self.save_to_overlay(request, conversation, path, content),
            Command::ListWorkspaceChanges => {
                let ws = self.workspace.clone();
                for meta in self.store.list(&ws.id).0 {
                    self.emit_changes(&ws, meta.id);
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
                let home = self.home.clone();
                tokio::spawn(async move {
                    let result = if profile.kind == BackendKind::ClaudeCode {
                        claude_version(profile.claude_command(), &home)
                            .await
                            .map(|v| vec![v])
                    } else {
                        match resolve_api_key(&profile.api_key).await {
                            Err(e) => Err(e.to_string()),
                            Ok(key) => factory(&profile, key)
                                .list_models()
                                .await
                                .map_err(|e| e.to_string()),
                        }
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

    /// A chat-view save: into the conversation's pending changes, never the real file, and
    /// never while its turn runs (the agent's layer may be mounted, and Retry would undo it).
    fn save_to_overlay(
        &mut self,
        request: u64,
        id: ConversationId,
        path: PathBuf,
        content: String,
    ) {
        let ws = self.workspace.clone();
        let fail = |this: &Self, path: PathBuf, error: String| {
            this.emit(Event::OverlaySaveFailed {
                request,
                conversation: id,
                path,
                error,
            });
        };
        let Some(root) = ws.root.clone() else {
            let error = "Chat views need a folder workspace (File › Open Folder)".into();
            fail(self, path, error);
            return;
        };
        if self.claude.contains_key(&id) {
            fail(self, path, cc::BUSY.into());
            return;
        }
        if self.busy(id) {
            let error = "void is working in this chat — save to Disk, or wait".into();
            fail(self, path, error);
            return;
        }
        if self
            .conversation(id)
            .is_none_or(|c| c.workspace_id != ws.id)
        {
            fail(
                self,
                path,
                "That chat isn't in this workspace any more".into(),
            );
            return;
        }
        let rel = match path.strip_prefix(&root) {
            Ok(rel) => rel.to_path_buf(),
            Err(_) => path.clone(),
        };
        let inside = !rel.as_os_str().is_empty()
            && rel
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)));
        if !inside {
            let error = format!("{} is outside the workspace", path.display());
            fail(self, path, error);
            return;
        }
        let Some(ov) = self.overlay_for(&ws, id) else {
            fail(
                self,
                path,
                "Couldn't open the chat's pending changes".into(),
            );
            return;
        };
        let name = rel.display().to_string();
        match ov.write(&rel, content.as_bytes()) {
            Ok(()) => {
                // Retry undoes the agent's last turn, not this save; and the agent hears
                // about it with its next turn's input.
                if let Err(e) = ov.keep_in_checkpoint(&rel) {
                    self.emit(Event::Error(format!(
                        "Saved {name}, but a Retry may undo it: {e}"
                    )));
                }
                if let Err(e) = ov.note_user_edit(&rel) {
                    self.emit(Event::Error(format!(
                        "Saved {name}, but couldn't tell the agent: {e}"
                    )));
                }
                self.emit(Event::OverlaySaved {
                    request,
                    conversation: id,
                    path,
                });
            }
            Err(e) => {
                let error = format!("Couldn't save {name} to the chat's pending changes: {e}");
                fail(self, path, error);
            }
        }
        self.emit(Event::Changes {
            conversation: id,
            changes: ov.changes(),
        });
    }

    fn open_workspace(&mut self, ws: Workspace) {
        if let Err(e) = self.store.touch(&ws) {
            self.emit(Event::Error(format!("Couldn't record the workspace: {e}")));
        }
        let streaming: Vec<_> = self
            .streams
            .keys()
            .chain(self.claude.keys())
            .copied()
            .collect();
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
        if self.busy(id) {
            self.emit(Event::Error(
                "void is still working on this chat (already replying or running tools); stop it first.".into(),
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
        let c = self.begin_turn(&c);
        self.reply(c, false);
    }

    /// Ask the conversation's backend for the next reply (compacting first when needed).
    fn start_stream(&mut self, c: Conversation, mode: StreamMode) {
        // Safety net: Claude Code chats never reach a backend (or void's compaction). Turns
        // start through `reply`; this is only reached if the chat switched to Claude Code in
        // the middle of one of void's own turns, which then ends here.
        if self.is_claude(&c) {
            if mode == StreamMode::Reply {
                self.emit(Event::Error(
                    "This chat now uses Claude Code; send your message again to continue.".into(),
                ));
                self.end_turn(c.id);
            }
            return;
        }
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
                    // A summary of what Claude Code already saw isn't news to it.
                    if let Some(seen) = c.claude_upto.as_mut()
                        && at <= *seen
                    {
                        *seen += 1;
                    }
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
        if c.workspace_id != current && !self.claude.contains_key(&id) {
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

/// `<command> --version` outside the sandbox, for the Backends panel's Test. Never passes a
/// prompt, so it costs no tokens.
async fn claude_version(command: &str, home: &std::path::Path) -> Result<String, String> {
    let program = cc::resolve_command(command, home)?;
    let missing = || {
        format!(
            "`{command}` not found \u{2014} install Claude Code or set the full path in the profile's Command"
        )
    };
    let mut cmd = tokio::process::Command::new(&program);
    cmd.arg("--version")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = match tokio::time::timeout(Duration::from_secs(5), cmd.output()).await {
        Err(_) => return Err(format!("`{command} --version` timed out after 5 s")),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => return Err(missing()),
        Ok(Err(e)) => return Err(format!("couldn't run `{command}`: {e}")),
        Ok(Ok(o)) => o,
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().map(str::trim).find(|l| !l.is_empty());
    match line {
        Some(l) if out.status.success() => Ok(l.to_string()),
        _ => Err(format!("`{command} --version` failed ({})", out.status)),
    }
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
            command: None,
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

    #[cfg(unix)]
    fn fake_claude(dir: &std::path::Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("claude");
        std::fs::write(
            &path,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ] && [ $# -eq 1 ]; then echo '2.1.330 (Claude Code)'; else exit 3; fi\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn claude_profile(command: &str) -> Profile {
        Profile {
            kind: BackendKind::ClaudeCode,
            command: Some(command.into()),
            ..profile("cc", "")
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_profile_reports_version() {
        let mut r = rig();
        r.started();
        let bin = fake_claude(&r.root);
        r.handle
            .send(Command::TestProfile(claude_profile(bin.to_str().unwrap())));
        match r.until(|e| matches!(e, Event::ProfileTested { .. })) {
            Event::ProfileTested { profile_id, result } => {
                assert_eq!(profile_id, "cc");
                assert_eq!(result.unwrap(), vec!["2.1.330 (Claude Code)"]);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_profile_reports_missing_binary() {
        let mut r = rig();
        r.started();
        let missing = r.root.join("no-such-claude");
        let cmd = missing.to_str().unwrap().to_string();
        r.handle.send(Command::TestProfile(claude_profile(&cmd)));
        match r.until(|e| matches!(e, Event::ProfileTested { .. })) {
            Event::ProfileTested { result, .. } => {
                let e = result.unwrap_err();
                assert!(e.contains("not found"), "{e}");
                assert!(e.contains("install Claude Code"), "{e}");
                assert!(e.contains(&cmd), "{e}");
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
                home: None,
                claude_bridge: None,
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

    /// A Claude Code permission request for a chat whose turn is over (a late one, after Stop,
    /// or from a process that's going away) is refused at once and shows no dialog.
    #[test]
    fn late_claude_ask_is_denied() {
        use crate::claude_code::permission::{HostMsg, TURN_OVER};
        use crate::claude_code::turn::PermissionAsk;
        let mut r = rig();
        r.started();
        let c = r.new_conversation();
        let (stdin, mut lines) = mpsc::unbounded_channel();
        let ask = PermissionAsk {
            request_id: "late-1".into(),
            tool: "Bash".into(),
            input: serde_json::json!({"command": "ls"}),
            suggestions: None,
            title: None,
            description: None,
            blocked_path: None,
        };
        let _ = r.handle.tx.send(Msg::ClaudePermission(HostMsg::Cancel {
            conversation: c,
            request_id: "unknown".into(),
        }));
        let _ = r.handle.tx.send(Msg::ClaudePermission(HostMsg::Ask {
            conversation: c,
            ask,
            stdin,
        }));
        let deadline = Instant::now() + Duration::from_secs(5);
        let line = loop {
            if let Ok(l) = lines.try_recv() {
                break l;
            }
            assert!(
                Instant::now() < deadline,
                "the late request was never answered"
            );
            std::thread::sleep(Duration::from_millis(2));
        };
        assert_eq!(
            line,
            crate::claude_code::control_response_deny("late-1", TURN_OVER, false)
        );
        r.handle.send(Command::NewConversation);
        let mut asked = false;
        loop {
            match r.next() {
                Event::ApprovalNeeded(_) => asked = true,
                Event::ConversationCreated(_) => break,
                _ => {}
            }
        }
        assert!(!asked, "no dialog for a turn that's over");
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

    #[test]
    fn send_during_turn_is_rejected_and_turn_state_reported() {
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
        a.r.until(|e| matches!(e, Event::TurnState { active: true, .. }));
        let id = match a.r.until(|e| matches!(e, Event::ApprovalNeeded(_))) {
            Event::ApprovalNeeded(req) => req.id,
            _ => unreachable!(),
        };
        a.r.send(a.conv, "another thing");
        match a.r.until(|e| matches!(e, Event::Error(_))) {
            Event::Error(m) => assert!(m.contains("still working"), "{m}"),
            _ => unreachable!(),
        }
        a.r.handle.send(Command::Approve {
            id,
            decision: Decision::Once,
        });
        a.finish_with("ok");
        a.r.until(|e| matches!(e, Event::TurnState { active: false, .. }));
        let users = a
            .saved()
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .count();
        assert_eq!(users, 1, "the second message was refused, not queued");
    }

    #[test]
    fn save_to_overlay_writes_and_notes() {
        let mut a = agent_rig(
            vec![
                call(
                    "c1",
                    "edit",
                    serde_json::json!({"path": "a.txt", "old_string": "pineapple", "new_string": "mango"}),
                ),
                say("edited"),
                call("c2", "read", serde_json::json!({"path": "a.txt"})),
                say("read"),
            ],
            50,
            None,
            1000,
        );
        a.r.send(a.conv, "change it");
        a.finish_with("edited");
        a.r.until(|e| matches!(e, Event::TurnState { active: false, .. }));
        a.r.handle.send(Command::SaveToOverlay {
            request: 1,
            conversation: a.conv,
            path: "a.txt".into(),
            content: "mango and kiwi\n".into(),
        });
        let conv = a.conv;
        a.r.until(|e| {
            matches!(e, Event::OverlaySaved { request: 1, conversation, path }
                if *conversation == conv && path == std::path::Path::new("a.txt"))
        });
        a.r.until(|e| matches!(e, Event::Changes { conversation, .. } if *conversation == conv));
        assert_eq!(
            a.overlay().read(std::path::Path::new("a.txt")).unwrap(),
            b"mango and kiwi\n"
        );
        assert_eq!(
            std::fs::read_to_string(a.ws.join("a.txt")).unwrap(),
            "pineapple\n",
            "a chat-view save never touches the real file"
        );
        // A path outside the workspace is refused.
        a.r.handle.send(Command::SaveToOverlay {
            request: 2,
            conversation: a.conv,
            path: "../escape.txt".into(),
            content: "x".into(),
        });
        match a.r.until(|e| matches!(e, Event::OverlaySaveFailed { .. })) {
            Event::OverlaySaveFailed { request, error, .. } => {
                assert_eq!(request, 2);
                assert!(error.contains("outside"), "{error}");
            }
            _ => unreachable!(),
        }
        // The note reaches the agent with its next turn's input, not in a tool result.
        a.r.send(a.conv, "look again");
        match a.r.until(|e| matches!(e, Event::ToolFinished { .. })) {
            Event::ToolFinished { output, .. } => {
                assert!(!output.content.contains("The user edited"), "{output:?}");
                assert!(output.content.contains("mango and kiwi"), "{output:?}");
            }
            _ => unreachable!(),
        }
        a.finish_with("read");
        assert!(a.overlay().take_rejections().is_empty());
    }

    /// The text of the last message in a request.
    fn last_text(req: &ChatRequest) -> String {
        req.messages.last().map(Message::text).unwrap_or_default()
    }

    #[test]
    fn user_edit_note_arrives_with_next_turn_input() {
        let mut a = agent_rig(
            vec![say("first"), say("second"), say("third")],
            50,
            None,
            1000,
        );
        a.r.send(a.conv, "hi");
        a.finish_with("first");
        a.r.until(|e| matches!(e, Event::TurnState { active: false, .. }));
        a.r.handle.send(Command::SaveToOverlay {
            request: 1,
            conversation: a.conv,
            path: "a.txt".into(),
            content: "user text\n".into(),
        });
        a.r.until(|e| matches!(e, Event::OverlaySaved { .. }));
        // A turn without tools still hears about it, in its very first request.
        a.r.send(a.conv, "look again");
        a.finish_with("second");
        a.r.until(|e| matches!(e, Event::TurnState { active: false, .. }));
        {
            let reqs = a.requests.lock().unwrap();
            let t = last_text(&reqs[1]);
            assert!(t.starts_with("look again"), "{t}");
            assert!(
                t.contains("The user edited a.txt in your pending changes"),
                "{t}"
            );
        }
        // Rejections travel the same way, once each.
        a.r.handle.send(Command::RejectFile {
            conversation: a.conv,
            path: "a.txt".into(),
        });
        a.r.until(|e| matches!(e, Event::Changes { changes, .. } if changes.is_empty()));
        a.r.send(a.conv, "and now");
        a.finish_with("third");
        let reqs = a.requests.lock().unwrap();
        let t = last_text(&reqs[2]);
        assert!(t.contains("The user rejected your changes to a.txt"), "{t}");
        assert!(!t.contains("The user edited"), "delivered once: {t}");
        assert!(
            a.saved().messages[2].text().contains("The user edited"),
            "the note stays in the history"
        );
    }

    #[test]
    fn retry_keeps_user_chat_view_save() {
        let mut a = agent_rig(
            vec![
                call(
                    "c1",
                    "edit",
                    serde_json::json!({"path": "a.txt", "old_string": "pineapple", "new_string": "mango"}),
                ),
                say("edited"),
                say("retried"),
            ],
            50,
            None,
            1000,
        );
        a.r.send(a.conv, "change it");
        a.finish_with("edited");
        a.r.until(|e| matches!(e, Event::TurnState { active: false, .. }));
        a.r.handle.send(Command::SaveToOverlay {
            request: 1,
            conversation: a.conv,
            path: "a.txt".into(),
            content: "user version\n".into(),
        });
        a.r.until(|e| matches!(e, Event::OverlaySaved { .. }));
        // Retry undoes the agent's turn, not the user's save.
        a.r.handle.send(Command::Retry(a.conv));
        a.finish_with("retried");
        a.r.until(|e| matches!(e, Event::TurnState { active: false, .. }));
        let ov = a.overlay();
        assert_eq!(
            ov.read(std::path::Path::new("a.txt")).unwrap(),
            b"user version\n"
        );
        let changes = ov.changes();
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].kind, ChangeKind::Modified);
        let reqs = a.requests.lock().unwrap();
        assert!(
            last_text(reqs.last().unwrap()).contains("The user edited a.txt"),
            "the note matches what's in the layer"
        );
    }

    #[test]
    fn chat_view_save_refused_during_turn() {
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
        a.r.handle.send(Command::SaveToOverlay {
            request: 7,
            conversation: a.conv,
            path: "a.txt".into(),
            content: "hijack\n".into(),
        });
        match a.r.until(|e| {
            assert!(
                !matches!(e, Event::OverlaySaved { .. }),
                "saved during a turn"
            );
            matches!(e, Event::OverlaySaveFailed { .. })
        }) {
            Event::OverlaySaveFailed { request, error, .. } => {
                assert_eq!(request, 7);
                assert!(error.contains("void is working in this chat"), "{error}");
            }
            _ => unreachable!(),
        }
        let ov = a.overlay();
        assert!(!ov.upper().join("a.txt").exists(), "no overlay write");
        assert_eq!(
            ov.read(std::path::Path::new("a.txt")).unwrap(),
            b"pineapple\n"
        );
        assert!(
            ov.take_rejections().is_empty(),
            "no note for a refused save"
        );
        a.r.handle.send(Command::Approve {
            id,
            decision: Decision::Once,
        });
        a.finish_with("ok");
        a.r.until(|e| matches!(e, Event::TurnState { active: false, .. }));
    }

    #[test]
    fn list_workspace_changes_reports_every_chat_with_an_overlay() {
        let mut a = agent_rig(vec![say("hi")], 50, None, 1000);
        let other = a.r.new_conversation();
        let _quiet = a.r.new_conversation();
        let w = a.r.store.workspace_for(&a.ws).unwrap();
        for c in [a.conv, other] {
            Overlay::open(&a.r.store.overlay_dir(&w.id, &c), &a.ws)
                .unwrap()
                .write(std::path::Path::new("a.txt"), b"x\n")
                .unwrap();
        }
        a.r.handle.send(Command::ListWorkspaceChanges);
        let mut seen = std::collections::HashSet::new();
        while seen.len() < 2 {
            if let Event::Changes {
                conversation,
                changes,
            } = a.r.until(|e| matches!(e, Event::Changes { .. }))
            {
                assert_eq!(changes.len(), 1);
                seen.insert(conversation);
            }
        }
        assert!(seen.contains(&a.conv) && seen.contains(&other));
    }

    #[test]
    fn stop_during_bash_keeps_partial_output() {
        let rules = defaults_with(Rule {
            tool: "bash".into(),
            prefix: None,
            action: Action::Allow,
        });
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let ws = root.join("project");
        std::fs::create_dir_all(&ws).unwrap();
        let cfg = Config {
            active_profile: "agent".into(),
            profiles: vec![profile("agent", "http://fake")],
            ..Config::default()
        };
        let cfg_path = root.join("config/config.toml");
        save_config(&cfg_path, &cfg).unwrap();
        let store = Store::new(root.join("data"));
        let w = store.workspace_for(&ws).unwrap();
        Permissions { rules }
            .save(&store.permissions_path(&w.id))
            .unwrap();
        let seq = Arc::new(Seq {
            scripts: Mutex::new(
                vec![
                    call(
                        "b1",
                        "bash",
                        serde_json::json!({"command": "echo started; sleep 30"}),
                    ),
                    say("after"),
                ]
                .into(),
            ),
            requests: Arc::new(Mutex::new(Vec::new())),
        });
        let factory: BackendFactory =
            Arc::new(move |_p: &Profile, _k: Option<String>| seq.clone() as Arc<dyn Backend>);
        let handle = Engine::start_with_options(
            cfg_path,
            store.clone(),
            Arc::new(|| {}),
            factory,
            EngineOptions {
                step_limit: 50,
                sandbox: true,
                home: None,
                claude_bridge: None,
            },
        );
        let mut r = Rig {
            _dir: dir,
            root,
            store,
            handle,
            notified: Arc::new(AtomicUsize::new(0)),
            received: 0,
        };
        match r.until(|e| matches!(e, Event::SandboxStatus(_))) {
            Event::SandboxStatus(crate::sandbox::Availability::Ready { .. }) => {}
            other => {
                eprintln!("skipping: {other:?}");
                return;
            }
        }
        r.handle.send(Command::OpenWorkspace(Some(ws.clone())));
        r.until(
            |e| matches!(e, Event::WorkspaceOpened { workspace, .. } if workspace.root.is_some()),
        );
        let conv = r.new_conversation();
        r.send(conv, "run it");
        r.until(|e| matches!(e, Event::ToolOutput { text, .. } if text.contains("started")));
        let t = Instant::now();
        r.handle.send(Command::Stop(conv));
        match r.until(|e| matches!(e, Event::ToolFinished { .. })) {
            Event::ToolFinished { output, .. } => {
                assert!(output.content.starts_with("stopped"), "{output:?}");
                assert!(output.content.contains("started"), "{output:?}");
            }
            _ => unreachable!(),
        }
        r.until(|e| matches!(e, Event::TurnState { active: false, .. }));
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
        let saved = r.store.load(&w.id, &conv).unwrap();
        assert!(saved.messages.iter().any(|m| m.parts.iter().any(
            |p| matches!(p, Part::ToolResult { content, .. } if content.contains("started"))
        )));
    }
}
