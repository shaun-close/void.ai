# void.ai Claude Code Backend Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A "Claude Code" backend kind: a chat's turn runs the user's installed, unmodified `claude` inside the chat's sandbox (stream-json), with its edits in the overlay/Review, its permission prompts in void's approval dialog, and its transcript stored as normal void parts.

**Architecture:** `void-core` gains `BackendKind::ClaudeCode`, a long-lived `Sandbox::spawn`, and a `claude_code` module (argument building, stream-json parsing into internal events, permission bridge, turn runner). The engine's turn path branches on the chat's profile kind and hands Claude Code turns to that module; void's own agent loop, tools and compaction are untouched for other kinds. The GUI adds generic tool cards for Claude Code tool names and the profile fields.

**Tech Stack:** M1–M3a's stack; no new runtime. A fake `claude` (a test script) drives all non-live tests.

**Spec:** `docs/superpowers/specs/2026-11-23-claude-code-backend-design.md`

## Global Constraints

- void.ai never reads, writes, stores or forwards Claude credentials or `~/.claude*` contents; it only bind-mounts `~/.claude/` and `~/.claude.json` read-write for the `claude` process. The optional profile API key may be passed as `ANTHROPIC_API_KEY`.
- The `claude` binary is run unmodified; no bundling.
- `claude` always runs inside the sandbox; no unsandboxed fallback. bwrap missing or `[sandbox] network = false` → clear error.
- Workspace edits only reach the chat overlay (Review). Never the real workspace.
- Claude Code's own permission settings are the authority (no `--permission-mode` for Build; `plan` for Plan). void's `Permissions` rules are not applied to these chats; "Always allow" applies Claude Code's suggested update, never void's rules.
- While a Claude Code turn runs in a workspace: Review Accept/Reject (any chat in that workspace) and that chat's chat-view saves are refused by the engine; the sandbox cache lock is held for the turn.
- Session id = conversation UUID (`--session-id` first, `--resume` after).
- **Stop condition:** if `~/.claude.json` can't be saved through a single-file read-write bind (rename-over fails), stop and report BLOCKED; don't choose a fallback.
- Tests never spend tokens except `#[ignore]` live tests run by hand.
- Commit after each task (`Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`); `moon run :format :lint :test :build` green at every commit.

## Review Focus

1. **Stop while a permission request is pending** → the dialog closes, the process group dies, the partial reply is kept as `stopped`, no orphan `claude`. → `stop_during_permission_request` (T4).
2. **`claude` exits without a `result`** (crash, killed, auth failure printed to stderr) → `StreamFailed` with capped stderr (never env), overlay intact, chat usable next turn. → `crash_without_result_fails_turn` (T4).
3. **The Claude session is gone** (`~/.claude/projects/...` deleted, or resume error) → the turn starts a fresh session and prepends the stored transcript once, instead of failing forever. → `missing_session_restarts_with_transcript` (T4).
4. **Huge stream lines** (a multi-MB tool result, a long base64 image) → parsed without blocking the engine, stored output capped like void's tool output, the GUI stays responsive. → `huge_tool_result_is_capped` (T3).
5. **Two Claude Code chats at once / a Claude turn plus another chat's bash** → the second waits on the cache lock with a visible status, nothing deadlocks, Review refuses during the turn. → `second_turn_waits_for_lock`, `review_refused_during_claude_turn` (T4).

---

### Task 1: Profile kind, config and Backends panel

**Files:** `crates/void-core/src/config.rs`, `apps/void-gui/src/panels/backends.rs`, `crates/void-core/src/engine.rs` (TestProfile only).

**Interfaces (produces):**
- `BackendKind::ClaudeCode` serialised as `"claude-code"`.
- `Profile.command: Option<String>` (serde `command`, skip when None); `Profile::claude_command(&self) -> &str` → `"claude"` when unset.
- `TestProfile` for this kind runs `<command> --version` outside the sandbox (5 s timeout) and reports `ProfileTested` with the version line, or "`<command>` not found — install Claude Code". No tokens.

- [ ] **Step 1: tests:** `claude_code_profile_round_trips` (toml with `kind = "claude-code"`, `command`, `model`, `api_key_env`), `claude_command_defaults_to_claude`, `test_profile_reports_version` (fake binary prints `2.1.330 (Claude Code)`), `test_profile_reports_missing_binary`, kittest `backends_panel_hides_url_fields_for_claude_code` (base URL / context limit / max output / extra body hidden; command and model shown).
- [ ] **Steps 2–5:** fail, implement, pass + gate, commit `feat(core): Claude Code backend kind`.

---

### Task 2: `Sandbox::spawn` for long-lived processes

**Files:** `crates/void-core/src/sandbox.rs`.

**Interfaces (produces):**
- `pub struct SpawnPlan { overlay: Option<OverlayMounts /*workspace, upper, work, target_dir — as in RunPlan*/>, argv: Vec<OsString>, env: Vec<(OsString, OsString)>, rw_binds: Vec<PathBuf>, unhide: Vec<PathBuf>, cwd: PathBuf }` — `overlay = None` for Scratch (read-only view plus `rw_binds` and `cwd` bound writable). Refactor `RunPlan` to reuse `OverlayMounts` if that keeps one mount builder.
- `pub async fn spawn(&self, plan: SpawnPlan) -> Result<Spawned, String>`; `pub struct Spawned { child: tokio::process::Child /*stdin/stdout/stderr piped, own process group*/, _lock: OwnedMutexGuard<()> /*cache lock held while alive*/ }`; `Spawned::kill(&mut self)` = SIGTERM to the group, SIGKILL after 3 s.
- `bwrap_args` refactor so `run` and `spawn` share mount building; `unhide` paths are dropped from the effective hidden list for this process only; `rw_binds` are `--bind`; missing `rw_binds` targets are created (`~/.claude/` as dir, `~/.claude.json` as `{}` file only if absent).
- No timeout; environment scrubbed as for `run`, plus `plan.env`.

- [ ] **Step 1: tests:** `spawn_args_unhide_and_bind_claude_state` (args: `~/.claude` and `~/.claude.json` bound rw, other hidden paths still tmpfs/null), `spawn_keeps_void_agent_hiding` (`run` args unchanged), `spawn_streams_stdio` (real bwrap if available, else skipped with a printed reason: `cat` echoes stdin), `spawn_holds_cache_lock` (a concurrent `run` waits until the spawned child exits), `spawn_kill_kills_group` (child spawns a grandchild; both gone after kill), `spawn_scratch_without_overlay`.
- [ ] **Step 1b (stop-condition check):** an `#[ignore]` test `claude_json_rename_through_bind` that, inside `spawn` with the same binds, writes a temp file next to `~/.claude.json` and renames it over the bind target (a copy of the real file in a temp HOME, never the user's). Run it once. If the rename fails, STOP and report BLOCKED (Global Constraints).
- [ ] **Steps 2–5:** fail, implement, pass + gate, commit `feat(core): long-lived sandboxed processes`.

---

### Task 3: `claude_code` module — arguments and stream parsing

**Files:** create `crates/void-core/src/claude_code/{mod.rs,args.rs,stream.rs}`; test fixture `crates/void-core/tests/fake-claude` (executable script: reads stream-json from stdin, replays a scenario file named by `FAKE_CLAUDE_SCENARIO`).

**Interfaces (produces):**
- `pub struct TurnArgs { model: Option<String>, plan_mode: bool, session: SessionMode /*New(Uuid)|Resume(Uuid)*/, append_system: String }`; `pub fn build_args(&TurnArgs) -> Vec<String>` → `-p --input-format stream-json --output-format stream-json --include-partial-messages [--verbose if the CLI requires it for stream-json] --permission-prompts host [--permission-mode plan] [--model M] (--session-id U | --resume U) --append-system-prompt S` (exact flags confirmed against `claude --help` / Claude Code headless docs; record any extra required flag such as `--verbose`).
- `pub fn user_message(text: &str) -> String` (one stream-json line).
- `pub enum CcEvent { Init { session: String, model: String }, TextDelta(String), ThinkingDelta(String), Assistant { parts: Vec<Part> }, ToolResult { call_id: String, content: String, is_error: bool, parent: Option<String> }, PermissionRequest { request_id: String, tool: String, input: serde_json::Value, suggestions: Option<serde_json::Value> }, Result { usage: Usage, is_error: bool, message: Option<String> }, Other }` and `pub fn parse_line(&str) -> CcEvent` (unknown/invalid → `Other`, never panics). `parent_tool_use_id` is carried for subagent nesting.
- Tool-result content is capped with the same limit void's tools use (`sandbox::cap_output` or the agent's cap).

- [ ] **Step 1: tests:** `args_first_turn`, `args_resume_plan_model`, `parse_init_text_thinking_tool_use_result` (fixture lines from the docs' message shapes), `parse_subagent_parent_id`, `parse_permission_request`, `parse_unknown_line_is_other`, `huge_tool_result_is_capped` (5 MB line parses in < 200 ms debug, stored content capped).
- [ ] **Steps 2–5:** fail, implement, pass + gate, commit `feat(core): Claude Code stream-json parsing`.

---

### Task 4: Engine turns through Claude Code

**Files:** `crates/void-core/src/claude_code/turn.rs`, `engine.rs` (branch in the turn path; Retry; Stop; Review/save refusal), `model.rs` (`Conversation.claude_session: Option<Uuid>`, serde default None — the Claude Code session this chat resumes; the first session uses the conversation UUID).

**Interfaces / behaviour:**
- When the chat's effective profile is `ClaudeCode`, `begin_turn` (notes block already appended, M3a T8) hands the turn to `claude_code::run_turn(ctx)`, which: builds `SpawnPlan` (workspace overlay or Scratch dir `data_dir/claude-scratch/<conversation>`), `rw_binds`/`unhide` = `~/.claude/`, `~/.claude.json`, env `ANTHROPIC_API_KEY` from the profile's key source if set; writes the user message; maps `CcEvent`s to `StreamStarted`/`Delta`/`ToolStarted`/tool-finished/`StreamFinished`/`StreamFailed` and appends `Part`s to the assistant message; closes stdin after `Result`.
- First Claude Code turn of a conversation that already has messages: prepend "Earlier in this chat:" + the stored transcript rendered as text (latest compaction `Summary` + later messages, capped at 200 KB, oldest cut first) to the user message, once; `None` → `New(conversation uuid)`, `Some(u)` → `Resume(u)`; store `Some(u)` after `Init`.
- Resume failure (an error `Result` or exit whose stderr/`message` says the session wasn't found): retry the same turn once with `New(fresh uuid)` and the transcript prepend, then store the fresh uuid.
- Stop → `Spawned::kill`, partial reply kept with `stopped = true`, pending approvals for that chat cancelled.
- Retry → restore the overlay checkpoint (existing), then resume from before the retried turn if the CLI supports choosing the resume point (record what you used), else a new turn in the same session with the note "The previous attempt was undone; its file changes were reverted." prepended.
- While any Claude Code turn runs in a workspace: `AcceptFile/AcceptHunk/RejectFile/RejectHunk/AcceptAll/RejectAll` for chats in that workspace and `SaveToOverlay` for that chat → `Error`/`OverlaySaveFailed` "A Claude Code turn is running — try again when it finishes".
- Errors: binary missing; not signed in (auth error in `Result` or stderr) → "Claude Code isn't signed in — run `claude` in a terminal to sign in"; network disabled or bwrap missing; exit without `Result` → last 20 stderr lines, capped, env never included.
- Usage: `Result.usage` → `Usage` (prompt = input + cache read + cache creation) for the context meter.
- Permission requests in this task: answered "deny: approvals not wired yet" is NOT acceptable — Task 5 wires them; for this task the fake scenarios contain none.

- [ ] **Step 1: tests (fake claude, engine level):** `claude_turn_streams_and_stores_parts`, `claude_turn_edits_land_in_overlay` (fake writes `a.txt` in cwd → appears in `Changes`, real file untouched; requires bwrap, else skipped with reason), `first_claude_turn_prepends_transcript_once`, `resume_uses_session`, `missing_session_restarts_with_transcript`, `stop_kills_and_keeps_partial`, `crash_without_result_fails_turn`, `auth_error_message`, `review_refused_during_claude_turn`, `second_turn_waits_for_lock`, `scratch_claude_turn_runs_in_scratch_dir`, `retry_claude_turn`, `api_key_env_passed`.
- [ ] **Steps 2–5:** fail, implement, pass + gate, commit `feat(core): run chat turns through Claude Code`.

---

### Task 5: Permission bridge

**Files:** `crates/void-core/src/claude_code/permission.rs`, `turn.rs`, `engine.rs` (approval routing), `permissions.rs` (Decision variants if needed).

**Behaviour:** `--permission-prompts host`: each `CcEvent::PermissionRequest` becomes the existing `ApprovalNeeded` (tool name, a readable summary: `Bash` → command, `Edit`/`Write`/`MultiEdit` → path, else pretty JSON; through the Approver hook) and the answer is written back on stdin in Claude Code's control-response format: allow (with unchanged input), deny (with message), or allow + the request's suggested permission update when the user picks "Always allow" (only offered when suggestions exist). If the stdio host channel can't be used from a non-SDK host, implement the documented fallback (`--permission-prompt-tool` with a tiny MCP stdio server void spawns inside the sandbox, answering over an inherited pipe) and record why. void's `Permissions` rules are never consulted for these chats.

- [ ] **Step 1: tests:** `permission_request_shows_approval`, `allow_writes_control_response`, `deny_with_message`, `always_allow_sends_suggestion` (and is not offered without suggestions), `void_rules_not_consulted` (a void deny rule for `bash` doesn't block Claude's `Bash`), `stop_during_permission_request`.
- [ ] **Steps 2–5:** fail, implement, pass + gate, commit `feat(core): Claude Code permission prompts in void`.

---

### Task 6: GUI — Claude Code chats

**Files:** `apps/void-gui/src/panels/chat.rs` (tool cards), `state.rs`, `app.rs` (status bar), `render_screenshots`.

**Behaviour:** generic tool card for unknown tool names (name, pretty JSON args collapsed to one line + expandable, result text); special cases: `Bash` (command + output like void's bash card), `Edit`/`MultiEdit`/`Write` (path + short diff/preview), `Read`/`Glob`/`Grep` (one-line summary); subagent parts (with a parent id) nested and collapsed under the parent `Task`/`Agent` tool card. Status bar shows the Claude Code model from `Init` and "waiting for sandbox" while a turn waits on the lock. The approval dialog shows "Always allow" only when offered.

- [ ] **Step 1: tests:** kittest `claude_tool_cards_render` (Bash, Edit, unknown tool), `subagent_parts_nest_under_parent`, `status_shows_claude_model`, `always_allow_only_when_offered`.
- [ ] **Steps 2–5:** fail, implement, pass + gate, screenshot `claude-code-chat.png` (inspect), commit `feat(gui): Claude Code chats`.

---

### Task 7: Verification

- [ ] Full gate green; screenshots inspected.
- [ ] `#[ignore]` live test `live_claude_code_turn` (real `claude`, temp workspace, prompt "Create hello.txt containing OK, then reply DONE"): the reply streams, `hello.txt` is a pending change in Review, the real temp workspace has no `hello.txt`, `~/.claude.json` still parses. Run once by hand (spends a few tokens on the user's login); record the result in the ledger.
- [ ] Commit `test: Claude Code backend end-to-end checks`.
