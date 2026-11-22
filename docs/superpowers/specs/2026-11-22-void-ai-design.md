# void.ai — design

Status: approved by the user (2026-11-22).

## Intent

void.ai is the user's own AI coding assistant, built as an **AI-first IDE and editor**. Existing tools (opencode, IDE
plugins) aren't to their taste. They work in JetBrains IDEs with IdeaVim today, so Vim emulation is a must (macros and
movement matter most; gaps are acceptable). It should be **modular**: panels, backends, tools and agent behaviour plug
into a core instead of being wired into the app. Modular means **compile-time** (decided): Rust traits and
registries, with new backends, tools and panels added as code in this workspace, and agent profiles and prompts in
config. Runtime plugins (WASM, scripts) are not built now; the registries should be shaped so they could host them
later.

It runs against local models first: a Strata server (Qwen3.8-Flash-Next, 256K context) on this machine, plus
llama.cpp `llama-server` and Ollama, with hosted Anthropic-style APIs possible later.

## Goals

- A Rust workspace managed by moon, with a **UI-agnostic core library** and a GUI app now, a TUI app later.
- A GUI where **every panel docks, splits, tabs and floats** (Dear ImGui docking style). The only fixed elements are a
  menu bar at the top and a status bar at the bottom.
- **Multiple backends** through switchable profiles: OpenAI-compatible chat completions with streaming (Strata,
  llama-server, Ollama `/v1`) and Anthropic-style `/v1/messages`.
- The harness manages system prompts, context, history, compaction and tool calling, in the spirit of opencode.
- An agent that reads and edits files and runs commands with permission prompts and diff review (milestone 2).
- A built-in code editor with Vim emulation (milestone 3), then AI actions on a selection (milestone 4).

## Non-goals (for now)

- Embedding Neovim. Explicitly rejected.
- Ghost-text / fill-in-the-middle completion (later; needs a small fast model).
- LSP, a terminal panel, MCP, web fetch, the TUI (all later; the structure must leave room for them).
- Starting or stopping inference engines (Strata, llama-server, Ollama). void.ai is a client; only one engine holds the
  GPU at a time and the user manages that himself.
- Panels torn out into separate OS windows (egui_dock floats inside the main window).
- A runtime plugin system (see Intent).

## Anti-goals

- A core that knows about egui. If the TUI would have to copy logic out of the GUI crate, the split has failed.
- Logic hard-wired to one backend or one model (e.g. assuming Qwen's reasoning fields everywhere).
- API keys written into conversation files or logs.
- A fixed layout. Apart from the menu bar and status bar, nothing is pinned.

## Constraints

- Machine: Linux (Wayland, fish shell), a 16 GB GPU, a desktop CPU, plenty of RAM.
- Tooling: moon 2.5.0 via proto (`~/.proto/shims`). Rust toolchains installed include 1.99.1; eframe 0.37 and
  egui_dock 0.22 need Rust ≥ 1.95, so the workspace pins **1.99.1** (`rust-toolchain.toml` and `.prototools`).
- Strata: OpenAI-compatible at `http://127.0.0.1:8080/v1` (streaming, tool calls, reasoning in
  `delta.reasoning_content`, thinking toggled with `chat_template_kwargs.enable_thinking`) and Anthropic-style at
  `http://127.0.0.1:8080/v1/messages`. Start: `strata launch --preset daily --no-tui --yes`; stop:
  `strata launch --stop --preset daily`; key: `strata launch --show-key --preset daily`.
  Limits (from `~/.config/opencode/opencode.json`): context 262144, input 258048, output 32768; opencode compacts
  automatically with a 123048-token buffer.
- Ollama 0.30.10 at `http://127.0.0.1:11434` (`/v1` OpenAI-compatible; model `gemma4:e4b`).
- `a separate benchmarking project` is a separate project and is not touched.

## Approaches considered (GUI stack)

| Option | Trade-offs | Outcome |
|---|---|---|
| **egui + eframe 0.37 + egui_dock 0.22** | Pure Rust, no C++. egui_dock gives tabs, splits, drag-to-dock, floating windows and serialisable layouts. Already used by `~/Projects/darkside` (eframe). Floating windows stay inside the app window; no ligatures. | **Chosen by the user.** |
| Dear ImGui via `dear-imgui-rs` 0.22 | Real ImGui docking, multi-viewport (OS windows). C++ dependency behind a one-maintainer binding; weaker text rendering for an editor; multi-viewport unreliable on Wayland. | Rejected. |
| GPUI (Zed) 0.2 | Best text rendering; no docking (would have to be built); young, churning API. | Rejected. |
| iced, floem | No docking; layout-centric style. | Dropped early. |

Vim: **own editor + Vim engine in Rust**, written clean-room from Vim's documented behaviour behind a host
interface, including marks, visual-block and the jump list. Embedding Neovim was rejected by the user. Because
void.ai is open source, no code or tests are taken from other editors.

## Decided

- **Order of AI features:** agent edits (opencode-style) first, then selection actions; ghost text later.
- **Milestones:**
  1. Workspace, docking shell, streaming chat, conversation list, backend and settings panels. Backends:
     OpenAI-compatible and Anthropic.
  2. Agent tools, permission prompts, diff view.
  3. Editor with the Vim port, file tree, files opening as dock tabs.
  4. Selection actions in the editor, reusing the diff view.
- **Agent behaviour (milestone 2, agreed as proposed):**
  - A *workspace* is a project folder opened in void.ai. Tools are confined to it; paths outside ask first.
  - Tools: `read`, `list`/`glob`, `grep`, `edit` (exact-string replace), `write`, `bash` (timeout, streamed output).
  - Permissions: reads run freely; `edit`, `write`, `bash` ask, showing the diff or command, with *allow once* /
    *always for this workspace* / *deny*. Overridable in config.
  - Agent profiles = system prompt + allowed tools. Built-in **build** (all tools) and **plan** (read-only); users add
    their own.
  - Loop: the model calls tools until done, with a step limit; streams throughout; Stop button; reasoning shown in a
    collapsible block.
  - Context: base system prompt + workspace `AGENTS.md`/`CLAUDE.md` + environment info. Automatic compaction
    (summarising older turns) at a threshold, defaulting to opencode's settings. Context meter in the status bar.
- **Workspaces (decided):** conversations and the dock layout belong to a workspace, like JetBrains projects. A
  workspace is a project folder; a built-in **Scratch** workspace (no folder) covers plain chat. M1 therefore has
  File › Open Folder, a recent-workspaces list, and opening a folder given on the command line (`void-ai <path>`).
  At start the app reopens the last workspace (Scratch on first run). One workspace per window in M1.
- **Storage:** config in `~/.config/void.ai/config.toml` (backend profiles are global, not per workspace). Per-workspace
  data (conversations as JSON, dock layout, permission grants from M2) lives centrally under
  `~/.local/share/void.ai/workspaces/<id>/`, where `<id>` derives from the folder's canonical path, plus a small index
  of known workspaces (path, name, last opened). Nothing is written into the project folder itself (builder's call:
  keeps repos clean; can be revisited).
  A profile's API key is either the key, an environment variable name, or a command whose stdout is the key (e.g.
  `strata launch --show-key --preset daily`).
- **Location:** `~/Projects/void.ai`, a git repo published at github.com/shaun-close/void.ai under
  **MIT OR Apache-2.0** (originally private; changed before the first push).

## Architecture

```
void.ai/
  .moon/workspace.yml        projects: apps/*, crates/*
  .moon/toolchains.yml       rust (versionFromPrototools)
  .prototools                moon 2.5.0, rust 1.99.1
  rust-toolchain.toml        1.99.1 + clippy, rustfmt
  Cargo.toml                 workspace, shared dependency versions
  moon.yml                   workspace tasks: check, build, test, lint, format
  crates/
    void-core/               UI-agnostic harness (M1+)
    void-vim/                Vim engine (M3)
    void-editor/             buffer model: rope, edits, undo, syntax (M3), no UI
  apps/
    void-gui/                eframe + egui_dock app (M1+), binary `void-ai`
    void-tui/                later
```

### void-core

- **Model:** `Workspace { id, name, root: Option<PathBuf> }` (None = Scratch).
  `Conversation { id, workspace_id, title, created, updated, profile_id, messages }`. `Message { role, parts }`, where a
  part is text, reasoning, a tool call, or a tool result. Usage per assistant message (prompt/completion tokens) when
  the backend reports it.
- **Backend trait:** one async method that takes a provider-neutral request (system prompt, messages, tools,
  sampling, max output) and returns a stream of events: `TextDelta`, `ReasoningDelta`, `ToolCallDelta`, `Usage`,
  `Done(stop_reason)`, `Error`. Implementations:
  - `openai`: `/v1/chat/completions` with SSE, `stream_options.include_usage`, reasoning read from
    `delta.reasoning_content` (and `delta.reasoning` if present), tool-call deltas assembled by index.
  - `anthropic`: `/v1/messages` with SSE event types (`message_start`, `content_block_*`, `message_delta`), thinking
    blocks mapped to reasoning.
  - Each backend can list models (`/v1/models`) for the settings panel's "test connection".
- **Profiles:** `{ id, name, kind (openai|anthropic), base_url, api_key source, model, context_limit, max_output,
  extra_body (JSON merged into each request) }`. `extra_body` carries backend-specific switches such as
  `chat_template_kwargs.enable_thinking` without special-casing models. Seeded defaults: Strata (OpenAI), Strata
  (Anthropic), Ollama (`/v1`, gemma4:e4b), llama-server (`:8081`, placeholder).
- **Engine handle:** the UI talks to the core through a handle: it sends commands (send message, stop, new/rename/
  delete conversation, switch profile, save config) and drains events (stream deltas, state changes, errors). The
  core runs on its own tokio runtime; a `Notify` callback supplied by the UI wakes it (the GUI calls
  `ctx.request_repaint()`). Any UI—GUI or TUI—uses the same handle.
- **Context building:** assembles the system prompt (base + agent profile prompt + workspace files in M2) and the
  message list for a request; estimates tokens (reported usage when available, else ~4 chars/token) for the meter.
  Compaction is M2, but the token accounting it needs lands in M1.
- **Persistence:** conversations saved after each completed turn (and on stop); config read at start and written
  when changed in the settings panel.

### void-gui

- `eframe` (glow renderer, Wayland + X11) with `egui_dock::DockArea` filling the space between a top
  `MenuBar` and a bottom status bar.
- Panels (dock tabs) in M1:
  - **Chat:** messages of the active conversation, Markdown-ish rendering (code blocks monospace), collapsible
    reasoning, streaming cursor, input box (Enter sends, Shift+Enter newline), Stop button, per-chat profile picker.
  - **Conversations:** list, new, rename, delete, select.
  - **Backends:** profiles list and editor, test connection (lists models), set active.
  - **Settings:** general app settings (theme, font size, default profile, base system prompt).
- Menu bar: File (Open Folder…, Recent workspaces, Scratch, New chat, Quit), View (show any closed panel, Reset layout), Backend (switch active profile).
- Status bar: current workspace, active profile and model, connection/streaming state, tokens/s of the last reply, context meter
  (used / limit).
- The dock layout is saved and restored between runs. Closed panels can be reopened from View.

## Milestone 1 — done means

1. `moon run :build` (or `void-gui:build`) builds the workspace; `moon run :check`, `:lint`, `:test` pass.
2. `moon run void-gui:run` launches the app with the menu bar, the status bar and a default layout containing the
   Chat, Conversations, Backends and Settings panels; each can be dragged, split, tabbed, floated and closed, and the
   layout survives a restart.
3. With Strata running, a chat through the seeded Strata (OpenAI) profile streams its reply (reasoning shown
   separately) and can be stopped mid-stream; the Strata (Anthropic) profile also streams.
4. Profiles can be edited and switched at runtime; conversations persist across restarts.
5. Opening a folder (menu or command line) switches to that workspace's conversations and layout; Scratch and recent
   workspaces work; each workspace's conversations and layout are kept separately.
6. Core has unit tests for SSE parsing and event mapping of both backends (recorded fixtures, no live server), plus an
   ignored-by-default live test against Strata.

## Builder's calls (from the review; the user can override)

- **Plan scope:** the implementation plan covers milestone 1 in detail. Milestones 2–4 get their own design/plan pass
  (acceptance criteria, M4's action list) when they start; M1 only has to leave room for them.
- **Concurrency:** every engine command and event carries a conversation ID from M1. Several Chat tabs can be open on
  different conversations, and each conversation can have one stream in flight at once; several conversations can
  stream at the same time (the server queues or runs them in parallel). The status bar shows the focused Chat tab's
  state; Stop applies to that conversation.
- **Live verification:** before starting Strata, check nothing else holds the GPU (llama-server, benchmarking project runs,
  Strata already up). If something does, stop and ask the user. Otherwise start Strata with the daily preset, verify, and
  ask before stopping it.

## Left to the builder

- Crate choices for HTTP/SSE (e.g. reqwest + a small SSE parser), serialisation, IDs, paths (`directories`).
- Exact panel visuals, theme details and fonts (dark theme by default).
- How Markdown is rendered in M1 (minimal is fine; a full renderer can come later).
- Internal module layout of each crate.
- Git: `git init` in the new folder, commit per completed task on `main`.
