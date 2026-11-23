# void.ai — Claude Code backend

## Intent

The user wants a fallback for machines where local AI isn't an option. The fallback is **Claude Code itself**: its own tools, skills, subagents, hooks and
`CLAUDE.md`, driven from inside void.ai so a chat works the same as any other void.ai chat — streaming replies,
tool cards, approvals, pending changes in Review, Retry, Stop, the context meter.

The user said shelling out to the `claude` binary is fine "as long as everything works the same".

## Goals

1. A new backend kind, **Claude Code**, selectable like any profile: as the machine's active (default) profile
   in `config.toml`, or per chat from the existing profile switch.
2. A Claude Code chat runs the user's installed, unmodified `claude` program. void.ai never reads, stores or
   forwards Claude credentials; `claude` uses whatever the user signed in with (subscription or API key).
3. **Edits go through Review.** `claude` runs inside the chat's sandbox, so its file writes land in that chat's
   overlay as pending changes, exactly like void's own agent.
4. Claude Code's permission prompts appear in void's approval UI. Claude Code's own permission settings decide
   what needs asking (see Permissions) — the chat behaves like Claude Code in a terminal.
5. The chat transcript shows Claude Code's text, thinking (if any), tool calls and results as normal void parts,
   stored in the conversation file, so switching the chat back to a local profile later still has the history.
6. Stop, Retry, the context meter, chat views (M3a Task 8) and "notes from void" keep working.

## Non-goals

- The official Agent SDK libraries (Python/TypeScript). Anthropic requires API-key auth for them, which rules out
  the user's subscription; they also only drive this same binary.
- Automatic fallback when the local backend is unreachable (the user chose a per-machine default).
- Reimplementing Claude Code's tools, prompts or compaction in void.ai.
- Running `claude` outside the sandbox.
- Windows/macOS (void.ai's sandbox is Linux-only).

## Anti-goals (failure even if it "works")

- void.ai handling Claude login tokens in any way (reading `~/.claude/.credentials.json`, setting OAuth env
  vars, proxying requests).
- Claude Code editing the real workspace directly (bypassing Review).
- A silent switch of a chat to a paid model.
- Modifying or bundling the `claude` binary.

## Constraints

- Anthropic's Claude Code legal page: subscription OAuth is for "ordinary use of Claude Code"; it doesn't
  prevent "an end user from signing in to the unmodified Claude Code binary with their own Claude subscription";
  third parties may not offer Claude.ai login or route requests through plan credentials, nor "collect, store, or
  intermediate Claude.ai credentials or session tokens". The design keeps void.ai out of the credential path
  entirely: the user signs in with `claude` itself, through Anthropic's flow.
- An optional profile API key (existing `api_key` / `api_key_env` / `api_key_cmd`) may be passed to `claude` as
  `ANTHROPIC_API_KEY` — API-key auth is always allowed.
- Verified on the user's `claude` 2.1.330: `-p`, `--input-format stream-json`, `--output-format stream-json`,
  `--include-partial-messages`, `--session-id <uuid>`, `--resume <id>`, `--fork-session`, `--model`,
  `--permission-mode` (`acceptEdits`, `auto`, `bypassPermissions`, `manual`, `dontAsk`, `plan`),
  `--permission-prompts host|none` ("host" = the SDK host over stdio, or `--permission-prompt-tool`),
  `--mcp-config`, `--append-system-prompt`, `--add-dir`, `--setting-sources`, `--bare`.
- void's sandbox (`crates/void-core/src/sandbox.rs`): bwrap with `--ro-bind / /`, the chat overlay on the
  workspace, hidden paths behind tmpfs, `network` setting (default on), per-command timeout clamped to 600 s.

## Approaches considered

| Option | Trade-offs | Outcome |
|---|---|---|
| **A. Run `claude -p` (stream-json) as a subprocess inside the chat's sandbox** | No new runtime; unmodified binary with the user's own login; overlay → Review works; one more process type for the sandbox to support (long-lived, piped, needs `~/.claude` writable) | **Chosen** (only sensible option) |
| B. Agent SDK in a Node/Python sidecar | Typed API, but API-key only (no subscription), adds a runtime, drives the same binary | Rejected |
| C. Run `claude` on the real workspace, outside the sandbox | Simplest; but edits skip Review and the sandbox | Rejected (breaks "works the same") |
| D. Reuse the Anthropic API backend | Already exists; but it's void's agent, not Claude Code's tools/skills/CLAUDE.md | Rejected (not what was asked) |
| Automatic fallback vs per-machine default | Auto is hands-off but can silently move a chat to a paid model mid-task | **Per-machine default** (user's choice) |

## Design

### Configuration

- `BackendKind::ClaudeCode` (`kind = "claude-code"` in `config.toml`). Fields used: `name`, `model` (passed as
  `--model` when non-empty, e.g. `opus`, `sonnet`, or a full model id; empty = Claude Code's own default),
  optional API key source, and a new optional `command` (path to `claude`; default `claude` on `PATH`).
  `base_url`, `context_limit`, `max_output`, `extra_body` are ignored for this kind (Backends panel hides them).
- The machine default is the existing active profile; per-chat override is the existing
  `SetConversationProfile`. No new switching mechanism.
- Backends panel "Test" for this kind: runs `claude --version` (outside the sandbox) and reports the version or
  "not found". It does not spend tokens. Sign-in problems surface on the first turn as a clear error with the hint
  "run `claude` in a terminal to sign in".

### Turn lifecycle (engine)

The engine's turn path branches on the chat's profile kind. For `ClaudeCode`, void's own agent loop, tools,
compaction and Backend trait aren't used; a new module `crates/void-core/src/claude_code.rs` runs the turn.

- **One `claude` process per turn**, started at `begin_turn` and ended when it emits its `result` message (or on
  Stop/error). Command:
  `claude -p --input-format stream-json --output-format stream-json --include-partial-messages
   --permission-prompts host [--permission-mode plan] [--model M]
   (--session-id <conversation uuid> | --resume <conversation uuid>)
   --append-system-prompt <void note>`
  The user message (plus any "[Notes from void]" block, M3a Task 8) is written to stdin as one stream-json user
  message; stdin stays open for permission answers and closes after `result`.
- **Session = conversation.** The conversation's UUID is used as Claude Code's session id
  (`--session-id` on the first Claude Code turn, `--resume` after). A new `Conversation.claude_session: bool`
  (or equivalent) records that the session exists.
- **Switching a chat to Claude Code mid-conversation**: the first Claude Code turn prepends the earlier transcript
  (rendered as plain text, using the latest compaction summary if any, capped) to the user message, under a
  heading saying it's the earlier part of this chat. Switching back to a local profile needs nothing: void
  already stores the Claude Code turns as normal parts.
- **Agents**: void's Plan agent → `--permission-mode plan`; Build → default mode. void's agent prompts aren't
  sent; `--append-system-prompt` carries only a short void note ("Your file changes are staged for the user's
  review in void.ai, not written to disk; the user accepts or rejects them.").
- **Settings**: default setting sources (user, project, local), so `~/.claude` skills, hooks, subagents, MCP
  servers and the project's `CLAUDE.md` / `.claude/` all load. Not `--bare`.

### Stream mapping

stream-json messages → void events and stored parts:

| Claude Code output | void |
|---|---|
| `system` init (session id, model, tools) | stream start; model name for the status bar |
| partial text / thinking deltas (`--include-partial-messages`) | `Delta` events (text / reasoning) |
| `assistant` message content: text, thinking, `tool_use` | `Part::Text`, `Part::Reasoning`, `Part::ToolCall { id, name, arguments }` + `ToolStarted` |
| `user` message with `tool_result` | `Part::ToolResult` + tool-finished event (output shown in the tool card) |
| subagent messages (`parent_tool_use_id` set) | nested under the parent tool card (collapsed by default) |
| `result` (usage, cost, is_error) | `StreamFinished` with `Usage`; context meter from input+cache tokens; error → `StreamFailed` |
| unparseable line | logged, ignored (forward-compatible) |

Tool names stay Claude Code's (`Read`, `Edit`, `Bash`, …); void's tool card renderer gains generic rendering for
unknown tool names (name + pretty JSON arguments + result text), with small special cases for `Edit`/`Write`
(path + diff-ish preview) and `Bash` (command + output).

### Permissions

- `--permission-prompts host`: Claude Code sends permission requests to the host over the stdio control channel
  (the same mechanism the Agent SDK uses). The builder confirms the exact message shapes from the Claude Code
  docs (headless / SDK permission docs); if the stdio host channel isn't usable from a non-SDK host, fall back to
  `--permission-prompt-tool` with a tiny MCP stdio server that void.ai spawns (inside the sandbox, talking back to
  the engine over an inherited pipe).
- *(Controller's call, from "works the same".)* **Claude Code's own permission system is the authority.** No
  `--permission-mode` flag is passed for Build, so the user's `~/.claude/settings.json` (`defaultMode`,
  allow/deny rules) and project settings decide, exactly as in a terminal; Plan passes `--permission-mode plan`.
  void's `Permissions` rules are not applied to Claude Code chats. Whatever Claude Code asks the host goes to
  void's approval dialog (`ApprovalNeeded` → `Approve`); Claude Code's own denials stand.
- The dialog's choices map onto Claude Code's answer: Allow once, Deny (with optional message), and — when Claude
  Code's request includes permission suggestions — "Always allow" applies Claude Code's suggested rule update
  (Claude's own settings), never void's lowercase `bash` rules, so void's own agent is unaffected.
- The Approver hook (future auditor) still sees every request that reaches the host.

### Sandbox

- A new `Sandbox::spawn` (alongside `run`) starts a long-lived process with piped stdin/stdout/stderr, its own
  process group, no fixed timeout (the turn owns its lifetime), the chat overlay on the workspace as for `run`,
  and access to Claude Code's own state: `~/.claude/` and `~/.claude.json`. **These are in the default `hidden`
  list on purpose (M2)**, so this is an explicit exception for the `claude` process — see "Claude Code's own
  config inside the sandbox" below.

### Claude Code's own config inside the sandbox (the user's decision)

- **Read-write, like the user's terminal.** For the `claude` process (and therefore everything it runs: Bash,
  hooks, MCP servers), `~/.claude/` and `~/.claude.json` are removed from the effective `hidden` list and bound
  read-write. Everything else in `hidden` stays hidden; the rest of the disk stays read-only; workspace edits
  still go to the overlay and Review. void's own agent keeps them hidden, unchanged.
- **Accepted risk (stated to the user):** a command Claude Code runs can read the Claude login token and can
  write hooks, skills or settings into `~/.claude` that later run outside the sandbox when `claude` is used in a
  terminal; those writes don't go through Review. This is the same exposure as running Claude Code in a terminal.
- void.ai itself still never reads or writes these files (anti-goal); it only bind-mounts them for `claude`.
- **Stop condition for the builder:** if Claude Code can't save `~/.claude.json` through a single-file bind mount
  (e.g. it renames a temp file over it and the rename fails), stop and report — don't pick a fallback (writable
  `$HOME`, a private `CLAUDE_CONFIG_DIR`, or copying the file back); each has a cost the user hasn't chosen.
- `cwd` is the workspace path, so Claude Code's project key in `~/.claude/projects/` is the real workspace path
  and sessions survive across void restarts.
- **Long-lived mounts** *(controller's call)*: the chat overlay stays mounted for the whole turn, so while a
  Claude Code turn runs in a workspace, Review's Accept/Reject (for any chat in that workspace) and chat-view
  saves for that chat are refused with "a Claude Code turn is running — try again when it finishes" (the engine
  checks, as for `SaveToOverlay` today). The sandbox's cache lock is held for the turn, so package-cache overlays
  work inside Claude's Bash; other chats' commands wait for it (the fallback case is mostly one chat at a time). The
  status bar shows why a command is waiting.
- **Scratch** *(controller's call)*: Scratch has no workspace, so a Claude Code chat there runs in an empty
  per-chat directory under void's data dir (inside the sandbox, no overlay), with tools; nothing reaches Review.
- **Extensions** *(controller's call)*: hooks, MCP servers and plugins load, but the sandbox stays as configured
  (desktop sockets, D-Bus, keyrings stay hidden; the rest of the disk read-only). Those that need more fail as
  tool errors in the chat; the existing `[sandbox] expose` list is how the user opens specific paths. The
  Backends panel's Test lists which of the user's MCP servers would see hidden paths, as a heads-up.
- Requires sandbox network (Claude Code calls the API). If `[sandbox] network = false` or bwrap is unavailable,
  the turn fails with a clear message; there is no unsandboxed fallback.
- Stop: SIGTERM to the process group, then SIGKILL after 3 s; the partial reply is kept as `stopped`.

### Retry

Retry restores the overlay checkpoint (as today) and re-asks from the session state before the retried turn:
resume with `--fork-session` from the turn before, if the CLI offers a way to pick the resume point; otherwise
the retry is sent as a new turn in the same session with a short note ("The previous attempt was undone; its
file changes were reverted."). The builder picks after checking the CLI docs and records the choice.

### Errors

- `claude` not found → error naming the profile's `command` and how to install.
- Not signed in / auth error in `result` → "Claude Code isn't signed in — run `claude` in a terminal to sign in".
- Rate limit / usage limit → shown as the reply's error, with Claude Code's own message.
- Process exits without `result` → `StreamFailed` with the last stderr lines (capped, never including env).

## Testing

- A fake `claude` (test script) that reads stream-json from stdin and replays canned stream-json, including
  partial deltas, tool use/results, a permission request, a subagent and an error `result`. Unit and engine tests
  run against it; no network, no real tokens.
- Tests: argument building (first turn vs resume, plan mode, model, API key env), stream mapping into parts and
  events, permission request → void rules → approval → answer on stdin, Stop kills the process group, mid-chat
  switch prepends the transcript once, sandbox `spawn` binds `~/.claude` read-write and keeps hidden paths hidden,
  missing binary and auth-error messages.
- One `#[ignore]` live test that runs the real `claude` in a temp workspace with a tiny prompt ("reply with OK"),
  run manually.

## What's left to the builder

- Exact stream-json and control-message field names (from the Claude Code docs for headless mode and the Agent
  SDK's message types), and the stdio-host vs MCP-tool permission route.
- The Retry resume-point mechanism (see above).
- Tool-card rendering details for Claude Code tool names.
- Whether `claude` needs any other writable path inside the sandbox (e.g. `~/.cache/claude`), found by running it.
