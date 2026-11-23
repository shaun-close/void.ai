//! Live checks against the installed `claude` and the user's own Claude Code login: real turns,
//! real tokens. Run by hand only:
//!
//! ```text
//! cargo test -p void-core --test claude_live -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The tests share one chat (and one engine) and send at most three real turns in all, whichever
//! of them run: the first turn creates a file, the second resumes the session, the third runs a
//! shell command (Claude Code may ask void's permission for it). Claude Code uses the real
//! `~/.claude/` and `~/.claude.json` — that's the feature — and never sees anything else of the
//! test's but its workspace: a fresh git repo under `~/.cache/void-ai-live-test/` (not under
//! `/tmp`, which is a tmpfs inside the sandbox; not under the repo, whose CLAUDE.md Claude Code
//! would pick up), deleted when the test process exits (`VOID_LIVE_KEEP=1` keeps it and prints
//! where).
//!
//! The profile's command is a wrapper around the real `claude` that copies its stdout (control
//! responses reduced to their envelope, so no account details) and stderr into a package-cache
//! dir, which the sandbox mounts with a host-visible layer, so the test can report the real
//! stream shapes.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as Proc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;
use void_core::claude_code::control_response_allow;
use void_core::claude_code::turn::{PermissionAsk, PermissionBridge, StdinTx, resolve_command};
use void_core::config::{ApiKeySource, BackendKind, Config, Profile, save_config};
use void_core::engine::{Command, DeltaKind, Engine, EngineHandle, EngineOptions, Event};
use void_core::model::{ConversationId, Part};
use void_core::overlay::Overlay;
use void_core::sandbox::{Availability, SandboxSettings};
use void_core::store::Store;

/// The most real turns one test process may send.
const TURN_CAP: usize = 3;
static TURNS_SENT: AtomicUsize = AtomicUsize::new(0);
/// The temp root, for the exit-time cleanup.
static ROOT: OnceLock<PathBuf> = OnceLock::new();
static LIVE: Mutex<Option<Live>> = Mutex::new(None);

/// Records Claude Code's permission requests and answers each "Allow once".
#[derive(Default)]
struct AllowOnce {
    asks: Mutex<Vec<PermissionAsk>>,
}

impl PermissionBridge for AllowOnce {
    fn ask(&self, _c: ConversationId, ask: PermissionAsk, stdin: StdinTx) {
        let _ = stdin.send(control_response_allow(&ask.request_id, &ask.input, None));
        self.asks.lock().unwrap().push(ask);
    }
    fn cancel(&self, _c: ConversationId, request_id: &str) {
        println!("[bridge] Claude Code withdrew request {request_id}");
    }
}

/// Everything one turn showed.
#[derive(Debug, Default, Clone)]
struct TurnLog {
    deltas: String,
    delta_count: usize,
    finished: Vec<String>,
    failed: Vec<String>,
    tools: Vec<(String, String)>,
    tool_results: Vec<(String, bool, String)>,
    model: Option<String>,
    statuses: Vec<String>,
    /// Permission requests that arrived during the turn: (tool, input).
    asks: Vec<(String, Value)>,
    secs: f32,
}

/// Where each step got to: None = not tried, Some(Err) = it failed (never retried: the cap).
#[derive(Default)]
struct Progress {
    turn: Option<Result<TurnLog, String>>,
    resume: Option<Result<TurnLog, String>>,
    bash: Option<Result<TurnLog, String>>,
}

struct Live {
    ws: PathBuf,
    rawlog: PathBuf,
    store: Store,
    handle: EngineHandle,
    bridge: Arc<AllowOnce>,
    chat: ConversationId,
    progress: Progress,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME"))
}

fn git(ws: &Path, args: &[&str]) {
    let ok = Proc::new("git")
        .args([
            "-c",
            "user.name=void-live",
            "-c",
            "user.email=void-live@invalid",
        ])
        .args(args)
        .current_dir(ws)
        .output()
        .expect("git")
        .status
        .success();
    assert!(ok, "git {args:?}");
}

/// The real `claude`, wrapped to copy its output into `rawlog` (see the module docs).
fn wrapper(root: &Path, rawlog: &Path, real: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = format!(
        r#"#!/bin/bash
log='{log}'
n=$(( $(ls "$log" | grep -c '^args-') + 1 ))
printf '%s\n' "$@" > "$log/args-$n"
env | cut -d= -f1 | sort > "$log/env-$n"
fo=/tmp/void-live-out-$n; fe=/tmp/void-live-err-$n
mkfifo "$fo" "$fe"
jq --unbuffered -R -c '(fromjson? // {{nonjson: .}}) | if .type == "control_response" then {{type, response: {{subtype: .response.subtype, request_id: .response.request_id, error: .response.error}}}} else . end' < "$fo" > "$log/stdout-$n.ndjson" &
j=$!
tee -a "$log/stderr-$n.txt" < "$fe" >&2 &
e=$!
'{real}' "$@" 2> "$fe" | tee "$fo"
code=${{PIPESTATUS[0]}}
wait $j $e
echo "$code" > "$log/exit-$n"
exit $code
"#,
        log = rawlog.display(),
        real = real.display(),
    );
    let path = root.join("bin/claude-logged");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// overlayfs leaves a mode-000 `work/work` behind, which stops a plain recursive delete.
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

extern "C" fn cleanup() {
    if let Some(root) = ROOT.get() {
        if std::env::var_os("VOID_LIVE_KEEP").is_some() {
            println!("[live] kept {}", root.display());
            return;
        }
        open_up(root);
        let _ = fs::remove_dir_all(root);
        // The shared parent too, once no other run is using it.
        let _ = root.parent().map(fs::remove_dir);
    }
}

impl Live {
    fn start() -> Option<Live> {
        let home = home();
        // VOID_LIVE_CLAUDE: wrap another program instead (a dry run of the harness, no tokens).
        let wanted = std::env::var("VOID_LIVE_CLAUDE").unwrap_or_else(|_| "claude".into());
        let Ok(real) = resolve_command(&wanted, &home) else {
            eprintln!("skipping: claude not installed");
            return None;
        };
        let base = home.join(".cache/void-ai-live-test");
        fs::create_dir_all(&base).unwrap();
        let root = tempfile::Builder::new()
            .prefix("claude-live-")
            .tempdir_in(&base)
            .unwrap()
            .keep();
        ROOT.set(root.clone()).unwrap();
        unsafe { libc::atexit(cleanup) };
        let ws = root.join("ws");
        let data = root.join("data");
        let rawlog = root.join("rawlog");
        for d in [&ws, &data, &rawlog] {
            fs::create_dir_all(d).unwrap();
        }
        fs::write(ws.join("README.md"), "# live test\n").unwrap();
        git(&ws, &["init", "-q"]);
        git(&ws, &["add", "."]);
        git(&ws, &["commit", "-q", "-m", "init"]);

        let command = wrapper(&root, &rawlog, &real);
        let mut sandbox = SandboxSettings::default();
        // As in real use, void's own data is hidden from the sandbox.
        sandbox.hidden.push(data.display().to_string());
        // The raw log's host-visible layer (a package cache, as far as the sandbox knows).
        sandbox.caches.push(rawlog.display().to_string());
        let profile = Profile {
            id: "cc".into(),
            name: "Claude Code".into(),
            kind: BackendKind::ClaudeCode,
            base_url: String::new(),
            api_key: ApiKeySource::None,
            model: String::new(),
            context_limit: 200_000,
            max_output: 32_000,
            extra_body: Default::default(),
            command: Some(command.display().to_string()),
        };
        let cfg = Config {
            active_profile: "cc".into(),
            profiles: vec![profile],
            sandbox,
            ..Config::default()
        };
        let cfg_path = root.join("config/config.toml");
        save_config(&cfg_path, &cfg).unwrap();
        let store = Store::new(data);
        let bridge = Arc::new(AllowOnce::default());
        let handle = Engine::start_with_options(
            cfg_path,
            store.clone(),
            Arc::new(|| {}),
            Arc::new(void_core::backend::make_backend),
            EngineOptions {
                claude_bridge: Some(bridge.clone()),
                ..EngineOptions::default()
            },
        );
        let mut live = Live {
            ws: ws.clone(),
            rawlog,
            store,
            handle,
            bridge,
            chat: ConversationId(uuid::Uuid::nil()),
            progress: Progress::default(),
        };
        match live.until(|e| matches!(e, Event::SandboxStatus(_))) {
            Event::SandboxStatus(Availability::Ready { landlock }) => {
                println!("[live] sandbox ready (landlock: {landlock})")
            }
            other => {
                eprintln!("skipping: sandbox unavailable: {other:?}");
                return None;
            }
        }
        live.handle.send(Command::OpenWorkspace(Some(ws)));
        live.until(
            |e| matches!(e, Event::WorkspaceOpened { workspace, .. } if workspace.root.is_some()),
        );
        live.handle.send(Command::NewConversation);
        live.chat = match live.until(|e| matches!(e, Event::ConversationCreated(_))) {
            Event::ConversationCreated(c) => c.id,
            _ => unreachable!(),
        };
        println!("[live] workspace {}", live.ws.display());
        Some(live)
    }

    /// Claude Code turns can be slow (hooks, thinking, tool calls): a generous per-event wait.
    fn next(&mut self) -> Event {
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            if let Some(e) = self.handle.try_recv() {
                return e;
            }
            assert!(Instant::now() < deadline, "timed out waiting for an event");
            std::thread::sleep(Duration::from_millis(5));
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

    /// Send one real turn and log everything until it's over.
    fn turn(&mut self, text: &str) -> TurnLog {
        let n = TURNS_SENT.fetch_add(1, Ordering::SeqCst) + 1;
        assert!(n <= TURN_CAP, "turn cap ({TURN_CAP}) reached");
        println!("\n[live] turn {n}: {text:?}");
        let c = self.chat;
        let asks_before = self.bridge.asks.lock().unwrap().len();
        let t = Instant::now();
        self.handle.send(Command::SendMessage {
            conversation: c,
            text: text.into(),
        });
        let mut log = TurnLog::default();
        loop {
            match self.next() {
                Event::Delta {
                    conversation,
                    kind: DeltaKind::Text,
                    text,
                } if conversation == c => {
                    log.delta_count += 1;
                    log.deltas.push_str(&text);
                }
                Event::StreamFinished {
                    conversation,
                    message,
                    ..
                } if conversation == c => log.finished.push(message.text()),
                Event::StreamFailed {
                    conversation,
                    error,
                } if conversation == c => log.failed.push(error),
                Event::ToolStarted {
                    conversation,
                    name,
                    summary,
                    ..
                } if conversation == c => log.tools.push((name, summary)),
                Event::ToolFinished {
                    conversation,
                    call_id,
                    output,
                } if conversation == c => {
                    let head: String = output.content.chars().take(160).collect();
                    log.tool_results.push((call_id, output.is_error, head));
                }
                Event::ClaudeInit {
                    conversation,
                    model,
                } if conversation == c => log.model = Some(model),
                Event::TurnStatus {
                    conversation,
                    status,
                } if conversation == c => log.statuses.push(status),
                Event::Error(e) => log.failed.push(format!("engine error: {e}")),
                Event::TurnState {
                    conversation,
                    active: false,
                } if conversation == c => break,
                _ => {}
            }
        }
        log.secs = t.elapsed().as_secs_f32();
        log.asks = self.bridge.asks.lock().unwrap()[asks_before..]
            .iter()
            .map(|a| (a.tool.clone(), a.input.clone()))
            .collect();
        println!("[live] turn {n} log: {log:#?}");
        self.print_raw(n);
        log
    }

    /// The real stream's shape for spawn `n`: each frame's type/subtype and keys (values only
    /// for a few harmless fields).
    fn print_raw(&self, n: usize) {
        let raw = fs::read_to_string(self.rawlog.join(format!("stdout-{n}.ndjson")));
        let raw = match raw {
            Ok(r) => r,
            Err(e) => {
                println!("[raw {n}] no stdout log ({e}) — cache layer not visible?");
                return;
            }
        };
        let mut last = String::new();
        let mut repeats = 0;
        let mut lines = Vec::new();
        for l in raw.lines() {
            let v: Value = serde_json::from_str(l).unwrap_or(Value::Null);
            let ty = v["type"].as_str().unwrap_or("?");
            let mut s = match ty {
                "stream_event" => format!(
                    "stream_event {} {}",
                    v["event"]["type"].as_str().unwrap_or("?"),
                    v["event"]["delta"]["type"].as_str().unwrap_or("")
                ),
                "control_request" | "control_response" | "control_cancel_request" => format!(
                    "{ty} {}",
                    v.get("request")
                        .or(v.get("response"))
                        .map(|r| format!(
                            "subtype={} {}",
                            r["subtype"],
                            r.as_object()
                                .map(|o| o.keys().cloned().collect::<Vec<_>>().join(","))
                                .unwrap_or_default()
                        ))
                        .unwrap_or_default()
                ),
                _ => format!(
                    "{ty} {} keys=[{}]",
                    v["subtype"].as_str().unwrap_or(""),
                    v.as_object()
                        .map(|o| o.keys().cloned().collect::<Vec<_>>().join(","))
                        .unwrap_or_default()
                ),
            };
            if ty == "system" && v["subtype"] == "init" {
                s.push_str(&format!(
                    " session_id={} model={} permissionMode={} version={}",
                    v["session_id"], v["model"], v["permissionMode"], v["claude_code_version"]
                ));
            }
            if ty == "result" {
                s.push_str(&format!(
                    " subtype={} is_error={} session_id={} num_turns={} usage_keys=[{}] \
                     permission_denials={}",
                    v["subtype"],
                    v["is_error"],
                    v["session_id"],
                    v["num_turns"],
                    v["usage"]
                        .as_object()
                        .map(|o| o.keys().cloned().collect::<Vec<_>>().join(","))
                        .unwrap_or_default(),
                    v["permission_denials"]
                        .as_array()
                        .map(|a| a.len())
                        .unwrap_or(0),
                ));
            }
            if ty == "assistant" || ty == "user" {
                let blocks: Vec<String> = v["message"]["content"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|b| b["type"].as_str().unwrap_or("?").to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                s.push_str(&format!(
                    " blocks={blocks:?} parent={} error={}",
                    v["parent_tool_use_id"], v["error"]
                ));
            }
            if s == last {
                repeats += 1;
                continue;
            }
            if repeats > 0 {
                lines.push(format!("  … ×{}", repeats + 1));
                repeats = 0;
            }
            lines.push(s.clone());
            last = s;
        }
        if repeats > 0 {
            lines.push(format!("  … ×{}", repeats + 1));
        }
        println!(
            "[raw {n}] {} frames:\n{}",
            raw.lines().count(),
            lines.join("\n")
        );
        let read = |f: &str| fs::read_to_string(self.rawlog.join(format!("{f}-{n}")));
        if let Ok(env) = read("env") {
            println!(
                "[raw {n}] env names: {}",
                env.lines().collect::<Vec<_>>().join(" ")
            );
        }
        if let Ok(args) = read("args") {
            let flags: Vec<&str> = args.lines().filter(|a| a.starts_with('-')).collect();
            println!("[raw {n}] flags: {}", flags.join(" "));
        }
        println!(
            "[raw {n}] exit: {}",
            read("exit").unwrap_or_default().trim()
        );
        let err =
            fs::read_to_string(self.rawlog.join(format!("stderr-{n}.txt"))).unwrap_or_default();
        let tail: Vec<&str> = err.lines().rev().take(15).collect();
        println!(
            "[raw {n}] stderr ({} lines), last: {:?}",
            err.lines().count(),
            tail.into_iter().rev().collect::<Vec<_>>()
        );
    }

    fn overlay(&self) -> Overlay {
        let w = self.store.workspace_for(&self.ws).unwrap();
        Overlay::open(&self.store.overlay_dir(&w.id, &self.chat), &self.ws).unwrap()
    }

    fn saved(&self) -> void_core::model::Conversation {
        let w = self.store.workspace_for(&self.ws).unwrap();
        self.store.load(&w.id, &self.chat).unwrap()
    }

    fn changes(&mut self) -> Vec<PathBuf> {
        self.handle.send(Command::ListChanges(self.chat));
        let c = self.chat;
        match self.until(|e| matches!(e, Event::Changes { conversation, .. } if *conversation == c))
        {
            Event::Changes { changes, .. } => changes.into_iter().map(|ch| ch.path).collect(),
            _ => unreachable!(),
        }
    }

    /// Step 1: a file-writing turn.
    fn step_turn(&mut self) -> Result<TurnLog, String> {
        if let Some(r) = &self.progress.turn {
            return r.clone();
        }
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let log = self.turn("Create hello.txt containing OK, then reply DONE");
            self.check_turn(&log);
            log
        }))
        .map_err(panic_text);
        self.progress.turn = Some(r.clone());
        r
    }

    fn check_turn(&mut self, log: &TurnLog) {
        assert!(log.failed.is_empty(), "turn failed: {:?}", log.failed);
        assert!(log.delta_count > 0, "no text deltas streamed");
        let last = log.finished.last().expect("a finished reply");
        assert!(last.contains("DONE"), "reply: {last:?}");
        let changes = self.changes();
        println!("[live] pending changes: {changes:?}");
        assert!(
            changes.iter().any(|p| p == Path::new("hello.txt")),
            "hello.txt not pending in Review: {changes:?}"
        );
        let body = self.overlay().read(Path::new("hello.txt")).unwrap();
        assert_eq!(String::from_utf8_lossy(&body).trim(), "OK");
        assert!(
            !self.ws.join("hello.txt").exists(),
            "hello.txt reached the real workspace"
        );
        let saved = self.saved();
        let parts: Vec<&Part> = saved.messages.iter().flat_map(|m| &m.parts).collect();
        assert!(
            parts.iter().any(|p| matches!(p, Part::ToolCall { .. })),
            "no stored tool call"
        );
        assert!(
            parts.iter().any(|p| matches!(p, Part::ToolResult { .. })),
            "no stored tool result"
        );
        let session = saved.claude_session.expect("claude_session stored");
        println!(
            "[live] stored claude_session {session} (chat id {})",
            saved.id.0
        );
        claude_json_parses();
    }

    /// Step 2: the same chat again, resumed.
    fn step_resume(&mut self) -> Result<TurnLog, String> {
        if let Some(r) = &self.progress.resume {
            return r.clone();
        }
        self.step_turn()
            .map_err(|e| format!("needs the first turn, which failed: {e}"))?;
        let before = self.saved().claude_session;
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let log = self.turn("What file did you just create? Reply with its name only.");
            assert!(log.failed.is_empty(), "turn failed: {:?}", log.failed);
            let last = log.finished.last().expect("a finished reply");
            assert!(last.contains("hello.txt"), "reply: {last:?}");
            assert_eq!(self.saved().claude_session, before, "session id changed");
            claude_json_parses();
            log
        }))
        .map_err(panic_text);
        self.progress.resume = Some(r.clone());
        r
    }

    /// Step 3: a shell command, which Claude Code may ask void about.
    fn step_bash(&mut self) -> Result<TurnLog, String> {
        if let Some(r) = &self.progress.bash {
            return r.clone();
        }
        self.step_resume()
            .map_err(|e| format!("needs the resume turn, which failed: {e}"))?;
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let log = self.turn("Run the shell command `touch made-by-bash` and reply DONE");
            assert!(log.failed.is_empty(), "turn failed: {:?}", log.failed);
            let last = log.finished.last().expect("a finished reply");
            assert!(last.contains("DONE"), "reply: {last:?}");
            if log.asks.iter().any(|(tool, _)| tool == "Bash") {
                println!("[live] stdio permission route exercised: Bash asked, allowed once");
            } else {
                println!(
                    "[live] stdio permission route not exercised by Bash (user settings allowed \
                     it); asks this turn: {:?}",
                    log.asks
                );
            }
            let changes = self.changes();
            println!("[live] pending changes: {changes:?}");
            assert!(
                changes.iter().any(|p| p == Path::new("made-by-bash")),
                "made-by-bash not pending: {changes:?}"
            );
            assert!(!self.ws.join("made-by-bash").exists());
            claude_json_parses();
            log
        }))
        .map_err(panic_text);
        self.progress.bash = Some(r.clone());
        r
    }
}

fn panic_text(p: Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<String>()
        .cloned()
        .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "panic".into())
}

/// `~/.claude.json` is still JSON after Claude Code saved through the sandbox (parsed only;
/// nothing of it is printed).
fn claude_json_parses() {
    let bytes = fs::read(home().join(".claude.json")).expect("~/.claude.json readable");
    if let Err(e) = serde_json::from_slice::<Value>(&bytes) {
        panic!(
            "~/.claude.json no longer parses (at line {}, column {})",
            e.line(),
            e.column()
        );
    }
}

/// The shared session, started on first use. None when claude or the sandbox isn't here.
fn with_live<T>(f: impl FnOnce(&mut Live) -> T) -> Option<T> {
    let mut g = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        *g = Some(Live::start()?);
    }
    Some(f(g.as_mut().unwrap()))
}

#[test]
#[ignore = "spends tokens on the user's Claude Code login; run by hand with --ignored"]
fn live_claude_code_turn() {
    let Some(r) = with_live(Live::step_turn) else {
        return;
    };
    r.unwrap();
}

#[test]
#[ignore = "spends tokens on the user's Claude Code login; run by hand with --ignored"]
fn live_claude_code_resume() {
    let Some(r) = with_live(Live::step_resume) else {
        return;
    };
    r.unwrap();
}

#[test]
#[ignore = "spends tokens on the user's Claude Code login; run by hand with --ignored"]
fn live_claude_code_tool_permission() {
    let Some(r) = with_live(Live::step_bash) else {
        return;
    };
    let log = r.unwrap();
    let asks = with_live(|l| l.bridge.asks.lock().unwrap().len()).unwrap_or(0);
    println!(
        "[live] permission requests over stdio in all turns: {asks}; this turn: {:?}",
        log.asks.iter().map(|(t, _)| t).collect::<Vec<_>>()
    );
}
