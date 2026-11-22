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

use crate::backend::{Backend, BackendError, StreamEvent, make_backend};
use crate::config::{Config, Profile, load_config, resolve_api_key, save_config};
use crate::context::{build_request, estimate_tokens};
use crate::model::{
    Conversation, ConversationId, ConversationMeta, Message, NEW_CHAT_TITLE, Part, Role, Workspace,
    WorkspaceId,
};
use crate::store::{Store, WorkspaceEntry};

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
    Stop(ConversationId),
    SaveConfig(Config),
    TestProfile(Profile),
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
    /// The full, current state of one conversation.
    Conversation(Conversation),
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
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_timeout(Duration::from_secs(1));
        }
    }
}

enum Msg {
    Cmd(Command),
    SaveLayout(WorkspaceId, String),
    Stream {
        conversation: ConversationId,
        generation: u64,
        item: StreamItem,
    },
}

enum StreamItem {
    Event(StreamEvent),
    Failed(String),
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
}

const TITLE_CHARS: usize = 40;

impl State {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>) {
        let (config, warning) = load_config(&self.config_path);
        self.config = config.clone();
        self.emit(Event::ConfigLoaded { config, warning });
        let last = self.store.last_workspace();
        self.open_workspace(last);
        while let Some(msg) = rx.recv().await {
            match msg {
                Msg::Cmd(cmd) => self.command(cmd),
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
            }
        }
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
                self.emit(Event::Conversation(c));
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
                self.emit_list();
            }
            Command::SetConversationProfile(id, profile) => {
                self.update(id, |c| c.profile_id = profile);
            }
            Command::SendMessage { conversation, text } => self.send_message(conversation, text),
            Command::Stop(id) => {
                if let Some(s) = self.streams.get(&id) {
                    s.abort.abort();
                    self.finish(id, true);
                }
            }
            Command::SaveConfig(config) => match save_config(&self.config_path, &config) {
                Ok(()) => {
                    self.config = config;
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
        let active = self.config.active_profile.clone();
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
        let profile_id = c.profile_id.clone().unwrap_or(active);
        let c = c.clone();
        self.save(&c);
        self.emit(Event::Conversation(c.clone()));
        self.emit_list();

        let generation = self.next_gen;
        self.next_gen += 1;
        let profile = self.config.profile(&profile_id).cloned();
        let request = profile.as_ref().map(|p| build_request(&c, p, &self.config));
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
            let (Some(profile), Some(request)) = (profile, request) else {
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
            let mut stream = factory(&profile, key).stream(request);
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
        self.emit(Event::StreamStarted(id));
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
        self.emit(Event::StreamFinished {
            conversation: id,
            message,
            tokens_per_sec,
        });
        self.emit_list();
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
            match self.until(|e| matches!(e, Event::Conversation(_))) {
                Event::Conversation(c) => c.id,
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
}
