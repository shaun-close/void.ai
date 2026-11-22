# void.ai Milestone 1 (Shell and Streaming Chat) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A moon-built Rust workspace whose `void-ai` GUI opens a workspace, shows a fully dockable layout (fixed menu bar and status bar only) and streams chats from Strata through switchable backend profiles.

**Architecture:** `crates/void-core` is a UI-agnostic library: data model, config and profiles, SSE backends (OpenAI-compatible, Anthropic), per-workspace storage, and an `Engine` that runs on its own tokio runtime and talks to any UI through a command/event handle. `apps/void-gui` is an eframe + egui_dock app that only renders state mirrored from engine events and sends commands.

**Tech Stack:** Rust 1.99.1 (edition 2024), moon 2.5.0 via proto, eframe 0.37 (glow, wayland, x11), egui_dock 0.22 (serde), tokio, reqwest (streaming), serde/serde_json/toml, futures-util, thiserror, uuid (v7), chrono, directories, sha2, rfd (folder picker), tempfile (dev).

**Spec:** `docs/superpowers/specs/2026-11-22-void-ai-design.md`

## Global Constraints

- Toolchain pinned to Rust **1.99.1** in `rust-toolchain.toml` and `.prototools` (with `moon = "2.5.0"`); edition 2024; `resolver = "3"`.
- Every cargo invocation in moon tasks uses `--locked`; `Cargo.lock` is committed.
- `void-core` must not depend on egui, eframe or any UI crate (anti-goal: a core that knows about egui).
- No logic special-cased by model name; backend-specific switches go through the profile's `extra_body`.
- API keys never appear in conversation files, logs, `Debug` output or error messages.
- Config: `~/.config/void.ai/config.toml`. Data: `~/.local/share/void.ai/` (index + `workspaces/<id>/`). Nothing is written into a project folder.
- Seeded Strata limits: context **262144**, max output **32768**. Strata OpenAI base URL `http://127.0.0.1:8080/v1`; Anthropic base URL `http://127.0.0.1:8080`; key command `strata launch --show-key --preset daily`; model id `qwen3.8-flash-next-iq3_xxs`.
- Ollama profile: base URL `http://127.0.0.1:11434/v1`, model `gemma4:e4b`. llama-server profile: base URL `http://127.0.0.1:8081/v1`, model `default`.
- Binary name `void-ai`. Dark theme by default.
- Do not touch `a separate benchmarking project`. Only one inference engine holds the GPU at a time.
- Commit after each task on `main`, ending messages with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. **Backend not running** (Strata stopped, connection refused): the chat shows an inline error on that conversation, the UI never blocks, and sending again works once the server is up. → `engine_reports_connection_refused` (Task 6).
2. **Bad or missing API key** (HTTP 401 with a JSON error body): the error names the status and the server's message, and never contains the key. → `openai_http_error_is_reported_without_key` (Task 3).
3. **Stop mid-stream**: the partial reply (text and reasoning so far) is kept, marked stopped, and saved to disk. → `stop_keeps_partial_reply` (Task 6).
4. **Corrupt config or conversation file**: the app still starts; the bad file is renamed to `<name>.bad-<unix-ts>` (never overwritten) and defaults or the remaining conversations load. → `corrupt_config_is_set_aside`, `corrupt_conversation_is_set_aside` (Tasks 2, 5).
5. **SSE split at awkward boundaries** (a multi-byte UTF-8 char or a line split across chunks, CRLF line endings, `: keepalive` comments, `data: [DONE]`): output is identical to receiving the stream in one chunk. → `sse_chunking_is_invisible` (Task 3).

---

## File Structure

```
void.ai/
  .gitignore  .prototools  rust-toolchain.toml  Cargo.toml  Cargo.lock  moon.yml
  .moon/workspace.yml  .moon/toolchains.yml
  crates/void-core/
    Cargo.toml  moon.yml
    src/lib.rs            re-exports
    src/model.rs          Role, Part, Message, Usage, Conversation, ConversationMeta, Workspace, ids
    src/config.rs         Config, Profile, BackendKind, ApiKeySource, load/save, defaults, key resolution
    src/fsutil.rs         atomic_write, set_aside (rename to .bad-<ts>)
    src/backend/mod.rs    ChatRequest, StreamEvent, StopReason, BackendError, Backend trait, make_backend
    src/backend/sse.rs    SseParser
    src/backend/openai.rs OpenAI-compatible backend
    src/backend/anthropic.rs Anthropic backend
    src/store.rs          Store: workspace index, conversations, layout blobs
    src/context.rs        build_request, estimate_tokens
    src/engine.rs         Engine, EngineHandle, Command, Event
    tests/fixtures/*.sse  recorded streams
    tests/live_strata.rs  #[ignore] live tests
  apps/void-gui/
    Cargo.toml  moon.yml
    src/main.rs           CLI args, eframe::run_native
    src/app.rs            VoidApp: eframe::App, menu bar, status bar, DockArea, event pump
    src/state.rs          UiState mirrored from engine events
    src/layout.rs         Tab enum, default_layout, ensure_tab
    src/panels/{mod,chat,conversations,backends,settings}.rs
```

---

### Task 1: Workspace scaffold

**Files:**
- Create: `.gitignore`, `.prototools`, `rust-toolchain.toml`, `Cargo.toml`, `moon.yml`, `.moon/workspace.yml`, `.moon/toolchains.yml`
- Create: `crates/void-core/{Cargo.toml,moon.yml,src/lib.rs}`, `apps/void-gui/{Cargo.toml,moon.yml,src/main.rs}`
- Already present: `docs/superpowers/specs/…`, `docs/superpowers/plans/…` (commit them here)

**Interfaces:**
- Produces: moon tasks `:check`, `:build`, `:test`, `:lint`, `:format` (root project `workspace`), `void-gui:run`; workspace deps table that later tasks extend.

Follow `~/Projects/darkside` (same moon 2.5 layout): `.moon/workspace.yml` with `projects.sources.workspace: '.'` and globs `apps/*`, `crates/*`; `vcs.defaultBranch: 'main'`; `versionConstraint: '2.5.0'`. `.moon/toolchains.yml`: `rust: { components: ['clippy','rustfmt'], versionFromPrototools: true, syncToolchainConfig: true }`. Root `moon.yml` tasks copied from darkside (`cargo check|build|test|clippy -D warnings|fmt --check`, all `--workspace --locked`, `cache: false`). `apps/void-gui/moon.yml` task `run: cargo run --package void-gui --locked --`. `.gitignore` as darkside's.

- [ ] **Step 1:** Write the files. `void-core/src/lib.rs` contains `pub fn version() -> &'static str { env!("CARGO_PKG_VERSION") }` and a test `version_is_set` asserting it is non-empty. `void-gui` `[[bin]] name = "void-ai"`, `main.rs` prints the core version.
- [ ] **Step 2:** Run `moon run :format :lint :test :build` (with `~/.proto/shims` on PATH; run `proto install` first if moon asks). Expected: all pass, `target/debug/void-ai` exists and prints `0.1.0`.
- [ ] **Step 3:** Commit everything including the docs: `chore: scaffold moon workspace with void-core and void-gui`.

---

### Task 2: Model and config

**Files:**
- Create: `crates/void-core/src/{model.rs,config.rs,fsutil.rs}`; modify `lib.rs`
- Test: unit tests in each module

**Interfaces:**
- Produces:
  - `pub struct ConversationId(Uuid)`, `pub struct WorkspaceId(String)` (both `Clone, Eq, Hash, Serialize, Deserialize, Display`); `ConversationId::new()` is uuid v7.
  - `pub enum Role { System, User, Assistant, Tool }`
  - `pub enum Part { Text(String), Reasoning(String), ToolCall { id: String, name: String, arguments: String }, ToolResult { call_id: String, content: String } }` (serde tag `"type"`)
  - `pub struct Usage { prompt_tokens: u32, completion_tokens: u32 }`
  - `pub struct Message { role: Role, parts: Vec<Part>, created: DateTime<Utc>, usage: Option<Usage>, stopped: bool, error: Option<String> }` with `Message::user(text)`, `Message::text(&self) -> String` (joins Text parts), `Message::reasoning(&self) -> String`
  - `pub struct Conversation { id, workspace_id, title: String, created, updated, profile_id: Option<String>, messages: Vec<Message> }`, `pub struct ConversationMeta { id, title, updated, profile_id }`, `Conversation::meta()`
  - `pub struct Workspace { id: WorkspaceId, name: String, root: Option<PathBuf> }`; `Workspace::scratch()` (id `"scratch"`, name `"Scratch"`, root None)
  - `pub enum BackendKind { OpenAi, Anthropic }` (serde `"openai"`, `"anthropic"`)
  - `pub enum ApiKeySource { None, Literal(String), Env(String), Command(String) }` — serde as optional fields `api_key`, `api_key_env`, `api_key_cmd` on the profile (at most one); `Debug` prints `Literal(***)`.
  - `pub struct Profile { id: String, name: String, kind: BackendKind, base_url: String, api_key: ApiKeySource, model: String, context_limit: u32, max_output: u32, extra_body: serde_json::Map<String, Value> }`
  - `pub struct Config { active_profile: String, base_system_prompt: String, theme: Theme, font_size: f32, profiles: Vec<Profile> }`; `Theme { Dark, Light }`; `Config::default()` seeds profiles; `Config::profile(&self, id) -> Option<&Profile>`
  - `pub fn load_config(path: &Path) -> (Config, Option<String>)` — returns defaults plus a warning when missing (no warning) or corrupt (warning, file set aside); `pub fn save_config(path: &Path, c: &Config) -> io::Result<()>`
  - `pub async fn resolve_api_key(src: &ApiKeySource) -> Result<Option<String>, KeyError>` — `Command` runs via `sh -c`, 10 s timeout, trims stdout; non-zero exit → `KeyError::Command { status, stderr }` (stderr truncated to 500 chars).
  - `fsutil::atomic_write(path, bytes) -> io::Result<()>` (write `path.tmp`, fsync, rename; creates parent dirs); `fsutil::set_aside(path) -> io::Result<PathBuf>` (rename to `<file>.bad-<unix-ts>`).

Seeded profiles (ids exact): `strata` (OpenAI, Strata values from Global Constraints, `api_key_cmd`, `extra_body = {"chat_template_kwargs": {"enable_thinking": true}}`), `strata-anthropic` (Anthropic, same key cmd/model/limits), `ollama` (OpenAI, no key, context 32768, max output 8192), `llama-server` (OpenAI, no key, context 32768, max output 8192). `active_profile = "strata"`, `font_size = 15.0`, `base_system_prompt` = one short paragraph ("You are void.ai, a coding assistant…"; exact wording builder's call).

- [ ] **Step 1: Write failing tests**
  - `config_roundtrips_through_toml`: `Config::default()` → `toml::to_string` → parse → equal.
  - `defaults_seed_strata`: `profile("strata")` has `base_url == "http://127.0.0.1:8080/v1"`, `context_limit == 262144`, `max_output == 32768`, `api_key == Command("strata launch --show-key --preset daily")`, `extra_body["chat_template_kwargs"]["enable_thinking"] == true`; `profile("strata-anthropic").base_url == "http://127.0.0.1:8080"`.
  - `literal_key_is_redacted_in_debug`: `format!("{:?}", Literal("sk-secret".into()))` does not contain `sk-secret`.
  - `key_sources_resolve`: Literal → `Some("k")`; Env (set var in test) → value; Command `"printf ' abc \n'"` → `Some("abc")`; Command `"exit 3"` → `Err(KeyError::Command{status: 3, ..})`; None → `Ok(None)`.
  - `missing_config_gives_defaults`: nonexistent path → defaults, warning None.
  - `corrupt_config_is_set_aside`: write `"not = [toml"`; `load_config` → defaults, warning Some, original path gone, a sibling `config.toml.bad-*` exists with the original bytes.
  - `message_text_joins_text_parts`: parts `[Reasoning("r"), Text("a"), Text("b")]` → `text() == "ab"`, `reasoning() == "r"`.
- [ ] **Step 2:** `cargo test -p void-core` → fails (types missing).
- [ ] **Step 3:** Implement the interfaces above.
- [ ] **Step 4:** `cargo test -p void-core` → pass; `moon run :lint` → pass.
- [ ] **Step 5:** Commit `feat(core): data model, config and profiles`.

---

### Task 3: SSE parser and OpenAI-compatible backend

**Files:**
- Create: `crates/void-core/src/backend/{mod.rs,sse.rs,openai.rs}`, `crates/void-core/tests/fixtures/{openai_text.sse,openai_reasoning.sse,openai_tools.sse}`

**Interfaces:**
- Consumes: `Profile`, `Message`, `Part`, `Role`, `Usage` (Task 2).
- Produces:
  - `pub struct ChatRequest { model: String, system: String, messages: Vec<Message>, max_output: u32, extra_body: Map<String, Value> }` (tools come in M2; leave no placeholder field).
  - `pub enum StreamEvent { TextDelta(String), ReasoningDelta(String), ToolCall { id: String, name: String, arguments: String }, Usage(Usage), Done(StopReason) }`; `pub enum StopReason { EndTurn, MaxTokens, ToolUse, Other(String) }`
  - `pub enum BackendError { Connect(String), Http { status: u16, message: String }, Protocol(String) }` (thiserror; `Display` is user-facing).
  - `pub trait Backend: Send + Sync { fn stream(&self, req: ChatRequest) -> BoxStream<'static, Result<StreamEvent, BackendError>>; fn list_models(&self) -> BoxFuture<'static, Result<Vec<String>, BackendError>>; }`
  - `pub fn make_backend(profile: &Profile, api_key: Option<String>) -> Arc<dyn Backend>`
  - `sse::SseParser::new()`, `fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent>`, `fn finish(&mut self) -> Vec<SseEvent>`; `pub struct SseEvent { event: Option<String>, data: String }`
  - `openai::request_body(req: &ChatRequest) -> Value`, `openai::OpenAiStream::default()`, `fn on_event(&mut self, ev: &SseEvent) -> Result<Vec<StreamEvent>, BackendError>`

Behaviour: POST `{base_url}/chat/completions` with `Authorization: Bearer <key>` when a key exists, body = `{model, messages, stream: true, stream_options: {include_usage: true}, max_tokens}` with `extra_body` merged on top (deep merge objects; `extra_body` wins). System prompt becomes the first `{"role":"system"}` message. Reasoning parts of past assistant messages are **not** sent back. Deltas: `choices[0].delta.content` → TextDelta, `delta.reasoning_content` or `delta.reasoning` → ReasoningDelta, `delta.tool_calls[*]` assembled by `index` and emitted as `ToolCall` when `finish_reason` arrives, `usage` → Usage, `finish_reason` `stop|length|tool_calls|other` → Done(EndTurn|MaxTokens|ToolUse|Other). `data: [DONE]` ends the stream; if the stream ends without any finish_reason, emit `Done(Other("eof"))`. Non-2xx: read body, take `error.message` if JSON else first 300 chars → `Http{status, message}`. `list_models`: GET `{base_url}/models`, `data[*].id`. Fixtures: hand-write realistic Strata-style chunks (reasoning then content, usage chunk with empty choices, `[DONE]`).

- [ ] **Step 1: Write failing tests**
  - `sse_basic`: `"data: a\n\ndata: b\nevent: x\n\n"` → two events, second has `event == Some("x")`… (field order irrelevant), multi-line `data` joined with `\n`, `: comment` lines ignored.
  - `sse_chunking_is_invisible`: for each fixture, feed whole vs. byte-by-byte vs. CRLF-converted byte-by-byte → identical event lists (fixtures include a multi-byte char like `é` or `→`).
  - `openai_body_merges_extra_body`: profile `extra_body {"chat_template_kwargs":{"enable_thinking":false},"temperature":0.2}` → body has both, `stream == true`, `stream_options.include_usage == true`, first message role `system`, no reasoning text from history.
  - `openai_maps_reasoning_fixture`: events concatenated → reasoning text, text, `Usage{..}` and `Done(EndTurn)` exactly as in fixture.
  - `openai_assembles_tool_calls`: tool-call fixture split across 3 deltas → one `ToolCall{ id, name, arguments: "{\"path\":\"a.rs\"}" }` then `Done(ToolUse)`.
  - `openai_streams_over_http`: a `tokio::net::TcpListener` mock on `127.0.0.1:0` replies with `content-type: text/event-stream` and the text fixture in several writes; assert received request has `authorization: Bearer k` and path `/v1/chat/completions`; collected events equal the fixture mapping.
  - `openai_http_error_is_reported_without_key`: mock replies `401 {"error":{"message":"invalid api key"}}`; error is `Http{status:401, message:"invalid api key"}` and `to_string()` doesn't contain the key `sk-test-123`.
  - `openai_lists_models`: mock `/v1/models` → `["qwen3.8-flash-next-iq3_xxs"]`.
- [ ] **Step 2:** `cargo test -p void-core backend` → fails.
- [ ] **Step 3:** Implement. Put the mock HTTP helper in `crates/void-core/tests/support/mod.rs` or a `#[cfg(test)] mod testsrv` so Task 4 reuses it.
- [ ] **Step 4:** Tests pass; lint passes.
- [ ] **Step 5:** Commit `feat(core): SSE parser and OpenAI-compatible streaming backend`.

---

### Task 4: Anthropic backend

**Files:**
- Create: `crates/void-core/src/backend/anthropic.rs`, `tests/fixtures/{anthropic_text.sse,anthropic_thinking.sse}`; modify `backend/mod.rs` (`make_backend` dispatch)

**Interfaces:**
- Consumes: `ChatRequest`, `StreamEvent`, `BackendError`, `SseParser`, mock server helper (Task 3).
- Produces: `anthropic::request_body(req) -> Value`, `anthropic::AnthropicStream` with the same `on_event` shape.

Behaviour: POST `{base_url}/v1/messages`, headers `x-api-key` (when key), `anthropic-version: 2023-06-01`, `content-type: application/json`. Body `{model, system, messages, max_tokens, stream: true}` + `extra_body` merge; roles user/assistant only, text parts as content strings. Events: `message_start` (usage.input_tokens), `content_block_start/delta` with `text_delta` → TextDelta, `thinking_delta` → ReasoningDelta, `tool_use` + `input_json_delta` assembled → ToolCall at `content_block_stop`, `message_delta` (`stop_reason` end_turn|max_tokens|tool_use, usage.output_tokens) → Usage then Done, `ping` ignored, `error` event → `Protocol(message)`. `list_models`: GET `{base_url}/v1/models`, `data[*].id`.

- [ ] **Step 1: Write failing tests** — `anthropic_body_has_system_and_version` (system top-level, not in messages; `max_tokens == profile max_output`), `anthropic_maps_thinking_fixture` (reasoning, text, `Usage{prompt_tokens, completion_tokens}` from message_start + message_delta, `Done(EndTurn)`), `anthropic_streams_over_http` (mock asserts `x-api-key`, `anthropic-version`, path `/v1/messages`), `anthropic_error_event_is_protocol_error`.
- [ ] **Step 2:** Run → fail. **Step 3:** Implement. **Step 4:** Run → pass, lint.
- [ ] **Step 5:** Commit `feat(core): Anthropic messages streaming backend`.

---

### Task 5: Per-workspace store

**Files:**
- Create: `crates/void-core/src/store.rs`

**Interfaces:**
- Consumes: `Workspace`, `Conversation`, `ConversationMeta`, `fsutil` (Task 2).
- Produces:
  - `pub struct Store { data_dir: PathBuf }`, `Store::new(data_dir)`, `Store::default_paths() -> (PathBuf /*config.toml*/, PathBuf /*data dir*/)` via `directories::ProjectDirs` with qualifier/org empty and app `void.ai` (resolving to `~/.config/void.ai` and `~/.local/share/void.ai`).
  - `pub fn workspace_for(&self, root: &Path) -> io::Result<Workspace>` — canonicalises; id = first 16 hex chars of sha256 of the canonical path; name = folder name.
  - `pub struct WorkspaceEntry { workspace: Workspace, last_opened: DateTime<Utc> }` (serde, Clone).
  - `pub fn recent(&self) -> Vec<WorkspaceEntry>`; `pub fn touch(&self, ws: &Workspace) -> io::Result<()>` (index `index.json`: `[{id, name, root, last_opened}]`, newest first, max 20; Scratch included); `pub fn last_workspace(&self) -> Workspace` (Scratch when none).
  - `pub fn list(&self, ws: &WorkspaceId) -> (Vec<ConversationMeta>, Vec<String> /*warnings*/)` sorted by `updated` desc; `load(&self, ws, id) -> io::Result<Conversation>`; `save(&self, c: &Conversation) -> io::Result<()>` (atomic); `delete(&self, ws, id)`.
  - `pub fn load_layout(&self, ws) -> Option<String>`; `save_layout(&self, ws, json: &str) -> io::Result<()>` (opaque JSON string owned by the UI; file `layout.json`).
  - Layout on disk: `<data>/workspaces/<id>/conversations/<conversation-id>.json`, `<data>/workspaces/<id>/layout.json`, `<data>/index.json`.

- [ ] **Step 1: Write failing tests** (tempdir data dir): `workspace_id_is_stable` (same folder via two paths, e.g. with `./`, → same id; different folders → different ids), `conversations_are_scoped_to_workspace` (save in A, list in B is empty), `save_load_roundtrip`, `list_sorts_newest_first`, `corrupt_conversation_is_set_aside` (garbage file → listed warnings has one entry, file renamed `*.bad-*`, the good conversation still listed), `recent_is_newest_first_and_capped` (21 touches → 20 entries), `last_workspace_defaults_to_scratch`, `layout_roundtrip_per_workspace`.
- [ ] **Step 2:** fail. **Step 3:** implement. **Step 4:** pass, lint.
- [ ] **Step 5:** Commit `feat(core): workspace-scoped conversation and layout store`.

---

### Task 6: Context building and the engine

**Files:**
- Create: `crates/void-core/src/{context.rs,engine.rs}`

**Interfaces:**
- Consumes: everything above.
- Produces:
  - `context::build_request(conv: &Conversation, profile: &Profile, cfg: &Config) -> ChatRequest` (system = `cfg.base_system_prompt`; messages = conversation messages excluding ones with `error` set and empty stopped ones).
  - `context::estimate_tokens(text: &str) -> u32` (= `ceil(chars / 4)`); `context::context_used(conv) -> u32` — last assistant message's `usage.prompt_tokens + completion_tokens` if present, else sum of estimates.
  - `pub type Notify = Arc<dyn Fn() + Send + Sync>`; `pub type BackendFactory = Arc<dyn Fn(&Profile, Option<String>) -> Arc<dyn Backend> + Send + Sync>`.
  - `pub struct Engine;` `Engine::start(config_path: PathBuf, store: Store, notify: Notify) -> EngineHandle` (uses `make_backend`); `Engine::start_with(…, factory: BackendFactory) -> EngineHandle` for tests. Owns a multi-thread tokio runtime on a background thread.
  - `pub struct EngineHandle`; `fn send(&self, cmd: Command)`; `fn try_recv(&self) -> Option<Event>`; `Drop` shuts the runtime down.
  - `pub enum Command { OpenWorkspace(Option<PathBuf>) /*None = Scratch*/, NewConversation, OpenConversation(ConversationId), RenameConversation(ConversationId, String), DeleteConversation(ConversationId), SetConversationProfile(ConversationId, String), SendMessage { conversation: ConversationId, text: String }, Stop(ConversationId), SaveConfig(Config), TestProfile(Profile) }`
  - `pub enum Event { ConfigLoaded { config: Config, warning: Option<String> }, WorkspaceOpened { workspace: Workspace, conversations: Vec<ConversationMeta>, recent: Vec<WorkspaceEntry>, layout: Option<String>, warnings: Vec<String> }, ConversationList(Vec<ConversationMeta>), Conversation(Conversation), StreamStarted(ConversationId), Delta { conversation: ConversationId, kind: DeltaKind /*Text|Reasoning*/, text: String }, StreamFinished { conversation: ConversationId, message: Message, tokens_per_sec: Option<f32> }, StreamFailed { conversation: ConversationId, error: String }, ProfileTested { profile_id: String, result: Result<Vec<String>, String> }, ConfigSaved, Error(String) }`
  - `EngineHandle::save_layout(&self, ws: &WorkspaceId, json: String)` (fire-and-forget via the store).

Behaviour: on start, emit `ConfigLoaded`, then open `store.last_workspace()` and emit `WorkspaceOpened`. `SendMessage`: append the user message, save, emit `Conversation`, resolve the profile (`conversation.profile_id` else `config.active_profile`), resolve its key, start a stream task; forward deltas as `Delta` events and accumulate the assistant `Message`; on Done set usage, save, emit `StreamFinished` with tokens/s = completion tokens (usage, else estimate) ÷ seconds from first delta to Done. A second `SendMessage` while that conversation streams → `Error("…already replying…")`; other conversations stream independently. `Stop`: abort the task, keep accumulated parts, `stopped = true`, save, emit `StreamFinished`. Backend error or key error → assistant message with `error` set, saved, `StreamFailed`. New conversation title = `"New chat"`, replaced by the first 40 chars of the first user message's first line. Call `notify()` after pushing every event. All storage errors become `Event::Error`, never panics.

- [ ] **Step 1: Write failing tests** (fake `Backend` whose stream yields scripted events with optional delays; tempdir store):
  - `build_request_skips_failed_messages`; `estimate_tokens_rounds_up` (`"abcde"` → 2).
  - `engine_streams_and_saves`: start → `ConfigLoaded`, `WorkspaceOpened` (Scratch); `NewConversation`; `SendMessage("hi there")` → `StreamStarted`, `Delta`s in order, `StreamFinished` whose message text is the scripted text; reloading from the store shows 2 messages and title `"hi there"`.
  - `stop_keeps_partial_reply`: fake streams 3 deltas then sleeps 10 s; after the 2nd `Delta` send `Stop` → `StreamFinished` within 1 s with `stopped == true` and the 2 deltas' text; saved on disk.
  - `engine_reports_connection_refused`: real `make_backend` with an OpenAI profile on a closed port (bind+drop a listener) → `StreamFailed` whose error mentions connecting; a following `SendMessage` to a working fake is accepted.
  - `two_conversations_stream_concurrently`: both `StreamStarted` before either `StreamFinished`.
  - `send_while_streaming_is_rejected`.
  - `workspace_switch_lists_its_conversations`: create a conversation in Scratch, `OpenWorkspace(Some(tempdir))` → `WorkspaceOpened` with empty list; back to Scratch → one conversation.
  - Use a helper `next_event(handle, timeout)` that polls `try_recv`; the notify callback counts calls and the count must be ≥ number of events received.
- [ ] **Step 2:** fail. **Step 3:** implement. **Step 4:** pass (run 5× to catch flakiness: `for i in 1 2 3 4 5; do cargo test -p void-core engine || break; done`), lint.
- [ ] **Step 5:** Commit `feat(core): context building and UI-agnostic engine`.

---

### Task 7: GUI shell — dock layout, menu bar, status bar

**Files:**
- Create: `apps/void-gui/src/{app.rs,state.rs,layout.rs,panels/mod.rs}`; modify `main.rs`, `Cargo.toml`

**Interfaces:**
- Consumes: `EngineHandle`, `Command`, `Event`, `Store::default_paths`.
- Produces:
  - `pub enum Tab { Chat { id: u32, conversation: Option<ConversationId> }, Conversations, Backends, Settings }` (serde; `id` distinguishes several Chat tabs).
  - `layout::default_layout() -> DockState<Tab>`: Conversations (left, ~22%), Chat (centre), Backends + Settings tabbed (right, ~28%).
  - `layout::ensure_tab(state: &mut DockState<Tab>, tab: Tab)` — focuses it if present, else pushes it to the focused leaf.
  - `state::UiState` (config, workspace, recent, conversation metas, loaded conversations by id, in-flight streams with partial text/reasoning, last tokens/s, profile test results, status message) with `fn apply(&mut self, ev: Event)`.
  - `panels::Panels` implementing `egui_dock::TabViewer<Tab = Tab>`; panels other than this task's are a placeholder label until Tasks 8–9.

Behaviour: `main.rs` parses an optional folder argument (`void-ai [PATH]`), creates the engine with `notify = ctx.request_repaint` (captured from `CreationContext`), and sends `OpenWorkspace(Some(path))` when given. Each frame: drain `try_recv` into `UiState`; top `MenuBar` (File: Open Folder… [rfd folder picker], Recent ▸, Scratch, New chat, Quit; View: one item per tab type that calls `ensure_tab`, Reset layout; Backend: radio list of profiles → sets `active_profile`, `SaveConfig`); bottom status bar (workspace name · active profile and model · `idle`/`streaming…`/error · last tokens/s · context `used / limit` for the focused Chat tab's conversation); `DockArea` fills the rest with close buttons and floating windows allowed. On `WorkspaceOpened`, restore `layout` JSON (fall back to default on parse error) ; save layout via `save_layout` when it changes (compare serialised JSON at most once per second) and on exit. Dark theme and font size from config. eframe window title `void.ai — <workspace>`; app id `void-ai`.

- [ ] **Step 1: Write failing tests** (`apps/void-gui`, plain unit tests, no window): `default_layout_has_all_tabs` (iterate `DockState::iter_all_tabs` → contains one each of Chat, Conversations, Backends, Settings), `ensure_tab_reopens_closed_tab` (remove Settings, ensure → present once), `ensure_tab_does_not_duplicate`, `layout_json_roundtrip`, `ui_state_applies_stream_events` (StreamStarted + 2 Delta + StreamFinished → conversation's last message equals finished message, no in-flight stream left).
- [ ] **Step 2:** fail. **Step 3:** implement.
- [ ] **Step 4:** tests + lint pass. Launch smoke: `timeout 5 target/debug/void-ai; echo $?` → `124` (still running, no panic). Screenshot with `grim` while running (Hyprland) for the record: `target/debug/void-ai & sleep 3; grim -g "$(hyprctl -j activewindow | jq -r '"\(.at[0]),\(.at[1]) \(.size[0])x\(.size[1])"')" /tmp/…/void-shell.png; kill %1` (save into the session scratchpad, not the repo).
- [ ] **Step 5:** Commit `feat(gui): docking shell with menu bar and status bar`.

---

### Task 8: Chat and Conversations panels

**Files:**
- Create: `apps/void-gui/src/panels/{chat.rs,conversations.rs}`

**Interfaces:**
- Consumes: `UiState`, `Tab::Chat`, `Command`.
- Produces: `chat::show(ui, state, tab_conversation: &mut Option<ConversationId>, input: &mut String, engine)`; `conversations::show(ui, state, dock, engine)`.

Behaviour:
- Conversations: list of metas (title, relative time), click → opens it in the most recently focused Chat tab (sets its `conversation`, sends `OpenConversation` if not loaded); context menu: Open in new tab, Rename (inline), Delete (confirm). "New chat" button → `NewConversation`, then the new conversation goes into the focused Chat tab.
- Chat: scrolling message list that sticks to the bottom while streaming; user and assistant messages visually distinct; assistant reasoning in a collapsed `CollapsingHeader` ("Thinking…" while streaming, "Thought" after); text rendered with fenced code blocks in monospace frames (other Markdown plain — builder's call); stopped messages tagged "stopped", errored ones show the error in the error colour with a Retry button (resends the last user text). Input: multiline, Enter sends, Shift+Enter newline; Send/Stop button swaps while streaming; per-conversation profile combo (defaults to "active profile"). A Chat tab with no conversation shows a hint and creates one on first send. Tab title = conversation title.
- Per-tab input text lives in the app (map tab id → String), not in the engine.

- [ ] **Step 1: Write failing tests** for the pure helpers this needs: `split_fenced_code` (`"a\n```rs\nfn x(){}\n```\nb"` → `[Text("a\n"), Code{lang:"rs", body:"fn x(){}\n"}, Text("b")]`; unterminated fence while streaming → code block to end), `tab_title` (None → `"Chat"`, long titles truncated to 30 chars with `…`).
- [ ] **Step 2:** fail. **Step 3:** implement helpers and panels.
- [ ] **Step 4:** tests + lint pass; launch smoke as in Task 7.
- [ ] **Step 5:** Commit `feat(gui): chat and conversation panels`.

---

### Task 9: Backends and Settings panels

**Files:**
- Create: `apps/void-gui/src/panels/{backends.rs,settings.rs}`

**Interfaces:**
- Consumes: `UiState.config`, `Command::{SaveConfig, TestProfile}`, `Event::ProfileTested`.
- Produces: `backends::show(ui, state, draft: &mut Option<Profile>, engine)`, `settings::show(ui, state, engine)`.

Behaviour:
- Backends: profile list (active one marked) with Add / Duplicate / Delete (not the active one); editor for the selected profile: name, kind (combo), base URL, key source (None / Literal [password field] / Env var / Command), model (text field plus a combo filled from the last Test result), context limit, max output, `extra_body` as a JSON text area with inline parse error (Save disabled while invalid). Buttons: Test connection (→ `TestProfile`, shows models or the error), Set active, Save (→ `SaveConfig`).
- Settings: theme, font size (10–24), default profile, base system prompt (multiline); Save → `SaveConfig`. Applying theme/font takes effect immediately.
- Profile id for new profiles: slug of the name, made unique with `-2`, `-3`….

- [ ] **Step 1: Write failing tests**: `profile_slug_is_unique` (`"Strata"` with existing `strata` → `strata-2`), `extra_body_parse` (valid object → Ok; `"[1]"` → Err "must be a JSON object"; invalid JSON → Err with message).
- [ ] **Step 2:** fail. **Step 3:** implement.
- [ ] **Step 4:** tests + lint pass; launch smoke.
- [ ] **Step 5:** Commit `feat(gui): backend profile and settings panels`.

---

### Task 10: Live verification against Strata

**Files:**
- Create: `crates/void-core/tests/live_strata.rs`

**Interfaces:**
- Consumes: `Config::default()`, `resolve_api_key`, `make_backend`, `Engine`.

- [ ] **Step 1:** Write `#[ignore]` tests `live_strata_openai_streams` and `live_strata_anthropic_streams`: build a `ChatRequest` "Reply with the word pong." with `max_output: 256`; assert at least one TextDelta, final text contains `pong` (case-insensitive), a `Usage` and a `Done`. Plus `live_strata_lists_models` (contains `qwen3.8-flash-next-iq3_xxs`).
- [ ] **Step 2: GPU check before starting anything.** Run `curl -s -m 2 http://127.0.0.1:8080/v1/models`, `pgrep -a llama-server`, `pgrep -af benchmark`, `ollama ps`. If Strata already answers, use it. If llama-server or a benchmark run is active, or Ollama has a model loaded, **stop and ask the user**. Otherwise start Strata: `strata launch --preset daily --no-tui --yes` (in the background) and poll `/v1/models` until it answers (up to 5 minutes).
- [ ] **Step 3:** `cargo test -p void-core --test live_strata -- --ignored --nocapture` → 3 passed.
- [ ] **Step 4: GUI end-to-end.** Run `moon run void-gui:run -- ~/Projects/void.ai`. Check by hand (driving it, screenshots with grim): the layout appears; drag a tab to split, tab two panels together, float one, close Settings and reopen from View; send "Write a haiku about Rust" via Strata (OpenAI) — reasoning collapses separately, text streams, status bar shows streaming → tokens/s and context meter; Stop during a longer prompt keeps the partial reply; switch the conversation to Strata (Anthropic) and send again; restart the app → same layout, conversations present; File › Scratch → different conversations and layout.
- [ ] **Step 5:** `moon run :format :lint :test :build` all pass. Commit `test(core): live Strata checks`. Ask the user before stopping Strata (`strata launch --stop --preset daily`).

---

## Self-review notes

- Spec coverage: workspace scaffold (T1); profiles, key sources, storage paths (T2, T5); both backends with streaming, reasoning, list models (T3, T4); concurrency with conversation IDs, stop, tokens/s, token accounting for compaction later (T6); docking/menu/status bar, layout per workspace, Open Folder/Recent/Scratch/CLI arg (T7); chat, conversations (T8); backends, settings (T9); done criteria 1–6 (T1, T7–T10).
- Not in M1 by design: tools, permissions, compaction, editor, Vim — milestones 2–4.
