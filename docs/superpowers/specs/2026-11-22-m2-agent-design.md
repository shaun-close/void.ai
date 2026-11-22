# void.ai milestone 2 — the agent: tools, sandbox, review panel

Status: approved by the user (2026-11-22). Builds on `2026-11-22-void-ai-design.md` (milestone 1, done).

## Intent

Milestone 2 turns void.ai from a chat into the user's coding agent. The agent reads the project, edits files and runs commands, in the spirit of opencode. Two things matter to the user beyond "it works":

- **They review changes in a panel, not in a stream of prompts.** Edits pile up as pending changes they accept or rejects, per file or per hunk, while the agent keeps going.
- **The protection is robust, not best-effort.** A confused model must not be able to `rm -rf` their disk, whatever it types. Later they want an *auto mode*, where a second model audits each action instead of them. Milestone 2 doesn't build auto mode, but it must leave a clean slot for it.

## Goals

- **An agent loop.** The model calls tools, void.ai runs them and returns the results, and this repeats until the model stops or hits a step limit. It streams throughout and can be stopped at any point.
- **Tools:**
  - `read` (a file, optionally a line range)
  - `list` (a directory)
  - `glob` (match paths by pattern)
  - `grep` (regex search)
  - `edit` (replace an exact string)
  - `write` (create or overwrite a whole file)
  - `bash` (run a command with a timeout and stream its output)
- **A per-conversation overlay.** Every change the agent makes, through `edit`, `write` or `bash`, lands in a pending layer belonging to that chat. The real workspace is untouched until the user accepts. The agent always sees its own pending changes.
- **A review panel** (dockable, like every panel). It shows each chat's pending changes as diffs and lets the user accept or reject per hunk, per file or all at once. It detects conflicts when the real file changed underneath.
- **An OS sandbox for `bash`.** Every command runs under bubblewrap:
  - The whole filesystem is read-only.
  - The workspace is mounted as the chat's overlay.
  - `/tmp` is private.
  - Sensitive folders are hidden.
- **Permissions.**
  - Rules per tool decide allow, ask or deny, with "always for this workspace" approvals.
  - Every decision goes through one approval hook, which the auditor model can answer later.
- **Agent profiles:** **build** (all tools) and **plan** (read-only tools), each with its own system prompt. Users can add their own.
- **Context:**
  - **System prompt:** the base prompt, plus the workspace's `AGENTS.md` / `CLAUDE.md`, plus environment information.
  - **Automatic compaction** near the context limit.

## Non-goals

- **Auto mode and the auditor model.** Only the hook ships now.
- **Background processes** such as dev servers left running after a `bash` call. Each call runs to completion or times out.
- **MCP, web fetch, LSP and a terminal panel.** These come later.
- **Tools in Scratch.** Scratch stays plain chat, because tools need a folder workspace.
- **Anything outside Linux.** the user runs Linux, which has bubblewrap 0.13, Landlock and user namespaces.

## Anti-goals

- **Safety that depends on recognising dangerous commands.** Blocklists like `rm -rf /` are trivially bypassed with `find / -delete` or `python -c …`. The guarantee must come from the OS: anything outside the workspace overlay is read-only.
- **Changes reaching the real workspace without the user's accept.** Today the user is the only one who accepts; later the auditor can too.
- **An agent that can't see its own pending edits.** That would produce confused follow-up edits.
- **Losing work.** Rejecting is the only action that discards a pending change. Accepting writes atomically and checks for conflicts first.

## Constraints and facts (verified 2026-11-22 on the user's machine)

- **Sandboxing:**
  - `bwrap` 0.13.1 at `/usr/bin/bwrap`, with `--overlay-src LOWER --overlay UPPER WORK DEST`.
  - Landlock is in `/sys/kernel/security/lsm`, and unprivileged user namespaces are enabled.
- **Experiment:** `bwrap --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp --overlay-src W --overlay U K W --chdir W bash -c …`, run with the workspace path itself as DEST, so the agent sees real paths.
  - Writes to `$HOME` and `/usr` fail with "Read-only file system". `/tmp` is writable and private.
  - The workspace (lower) is untouched.
  - In UPPER, a changed file is a full copy.
  - A deleted lower file becomes a **whiteout**: a character device `0:0` with the same name.
  - A removed and recreated directory becomes an **opaque** directory, marked with xattr `user.overlay.opaque="y"`. The overlay mounts with `userxattr`.
  - DEST must already exist, which it always does since it's the workspace itself.
  - UPPER and WORK must be on the same filesystem.
- **Tool calls:**
  - Strata serves tool calls in the OpenAI format (`tool_calls`, `role: tool`) and the Anthropic format (`tool_use` / `tool_result` blocks).
  - Milestone 1's backends already parse tool-call deltas into `StreamEvent::ToolCall`.
- **Milestone 1 rules still apply:** the core has no UI dependencies, keys never leave the profile, and data is stored per workspace under `~/.local/share/void.ai/workspaces/<id>/`.

## Approaches considered

| Decision | Options | Chosen (by the user) |
|---|---|---|
| **How edits are approved** | Review panel / inline approval per edit / apply automatically with undo | **Review panel** |
| **Where changes land** | **A.** An overlay that every tool, `bash` included, writes into, reviewed as diffs. **B.** Edits pending, `bash` writing straight into the workspace with snapshots for undo. | **A. Overlay** |
| **How many overlays** | One per conversation / one shared per workspace | **One per conversation** |
| **How safety is enforced** | Command blocklists / OS sandbox | **OS sandbox** (agreed in conversation; blocklists rejected as bypassable) |

## Design

### Overlay storage (void-core `overlay` module)

Each overlay lives at `~/.local/share/void.ai/workspaces/<ws>/overlays/<conversation-id>/` and contains:

- **`upper/`** — the overlay upper directory, in the format bwrap and overlayfs produce.
- **`work/`** — overlayfs's work directory. It's empty between commands.
- **`base/`** — for each changed path, the lower file's content as it was when the change was first made. Diffs are computed against `base`, and a conflict means "lower ≠ base".
  - For `edit` and `write`, void.ai records `base` itself.
  - For `bash`, after each command void.ai snapshots into `base` every upper path that has no base yet, copying the lower file at that moment. This is good enough: the command ran a moment ago.
- **`meta.json`** — when it was created, and the hunks the user rejected (reported back to the agent).

All of this lives on the data directory's filesystem, so UPPER and WORK share a filesystem as overlayfs requires. The workspace itself may be on another disk; lower can be anywhere.

**The merged view in userspace.** The `read`, `list`, `glob`, `grep`, `edit` and `write` tools run in the void-core process, not in bwrap, so they resolve paths the way overlayfs would:

- An upper whiteout (char device `0:0`) means the path doesn't exist.
- An upper file wins over the lower one.
- An upper directory marked opaque hides the lower directory's contents.
- Otherwise the lower file applies.

`write` and `edit` create the upper parent directories as needed. A plain directory is fine, because overlayfs merges it. They then write the file into upper. The file tools never create whiteouts, because none of them deletes; only `bash` can delete, and the kernel writes those whiteouts.

**Reviewing.** The changes are the upper entries, excluding work state, grouped by kind: added, modified, deleted, or replaced directory. Each gets a line diff against `base` (or against an empty file when added). Paths matched by the workspace's `.gitignore` (the `ignore` crate) are **hidden from review**. They are discarded with the overlay unless the user turns on "keep ignored files" when accepting.

**Accepting**, per file:

- **Conflict check.** If `base` exists and lower ≠ base, it's a conflict. Show both versions and refuse to accept until the user chooses "overwrite" or "reject".
- **Modified or added:** atomically write the upper content to lower, keeping lower's permissions or upper's mode for new files. Then remove the upper and base entries.
- **Deleted** (whiteout): delete the lower path, recursively for a directory, but only after confirming that path resolves inside the workspace. Then remove the whiteout.
- **Opaque directory:** delete lower entries that aren't in upper, then accept each upper entry.

**Per hunk:**

- **Accept:** lower = base with that hunk applied; base = the new lower. When upper equals lower, drop the upper entry.
- **Reject:** upper = upper with that hunk reverted. When upper equals base, drop the upper and base entries.
- Rejected hunks are recorded in `meta.json`. The next tool result the agent receives includes a short "The user rejected these changes: …" note.

**Discarding** a conversation's overlay deletes its directory. Deleting a conversation discards its overlay, after a confirmation that names how many pending changes exist.

### Sandbox for bash (void-core `sandbox` module)

One bwrap invocation per `bash` call, with this argument list:

```
bwrap --die-with-parent --new-session --unshare-pid --unshare-ipc --unshare-uts
      --ro-bind / /  --dev /dev  --proc /proc  --tmpfs /tmp
      [--tmpfs <hidden-dir>]...            # sensitive dirs, see below
      --overlay-src <workspace> --overlay <upper> <work> <workspace>
      --chdir <workspace>
      [--unshare-net]                      # when the profile turns network off
      --setenv HOME <real $HOME> ...       # a minimal, explicit environment
      -- bash -lc <command>
```

- **Hidden by default**, with an empty tmpfs over each path that exists. This stops a model from reading keys and sending them over the network. The list is configurable in config.
  - `~/.ssh`, `~/.gnupg`, `~/.aws`, `~/.netrc`
  - `~/.config/gh`, `~/.local/share/keyrings`
  - `~/.mozilla`, `~/.config/chromium`, `~/.config/google-chrome`
  - `~/.config/opencode`, `~/.config/void.ai`, `~/.local/share/void.ai`
  - `~/.claude`, `~/.config/Claude`
- **Environment.** Pass `PATH`, `HOME`, `LANG`, `TERM=dumb`, `USER` and `CARGO_HOME` / `RUSTUP_HOME`, plus `PROTO_HOME` if set. Nothing else: shell variables often hold API keys.
- **Network** is on by default, so builds can fetch crates; the agent profile can switch it off. This is the remaining risk: a command can still reach the network, which is why `bash` asks by default (see permissions).
- **Output.**
  - stdout and stderr are merged and streamed to the UI.
  - What the model gets back is capped: the last 30 KB, with a "…(N bytes cut)" note.
  - The timeout defaults to 120 s with a maximum of 600 s. When it expires, the process group is killed, which `--die-with-parent` and `--new-session` cover.
- **Start-up check.** If `bwrap` is missing or user namespaces are off, `bash` is unavailable. The tool says why, and the status bar shows a warning. Never fall back to running commands unsandboxed.
- **One command per overlay at a time.** Concurrent `bash` calls in the same chat are serialised. Different chats use different overlays.

### Tools (void-core `tools` module)

- **A `Tool` trait:** `name`, `description`, a JSON-Schema `parameters` object, `kind` (read / edit / exec), and `run(args, ctx) -> ToolOutput`.
  - `ctx` gives the tool its workspace root, its overlay view and a cancellation token.
  - Tools live in a **registry** that profiles select from, matching the compile-time modularity agreed for milestone 1.
- **Every path argument is confined to the workspace.**
  - Resolve it relative to the workspace and normalise it.
  - Reject `..` escapes.
  - Resolve symlinks through the merged view; a link pointing outside the workspace is refused for edit and write, and allowed for read.
- **Tool details:**
  - `read`: up to 2,000 lines, with line numbers, and refuses binary files.
  - `glob` and `grep` respect `.gitignore` and cap their output.
  - `edit` requires `old_string` to be unique unless `replace_all` is set.
  - `write` creates or overwrites a file.
  - `list` returns the merged directory listing.

### Agent loop (engine)

1. Build the request with the profile's tool definitions in the backend's format: OpenAI `tools`, or Anthropic `tools` with `input_schema`.
2. Stream the reply. On `Done(ToolUse)`, take every `ToolCall` from the reply.
3. For each call, in order: get a **permission decision** (below). If allowed, run the tool; if denied, return "denied by user". Then append a `Part::ToolResult`.
4. Send again with the results. Repeat until the reply ends without tool calls, the **step limit** (default 50 tool rounds) is reached, or the user presses Stop. Stop cancels a running tool, killing a running `bash`.
5. History is mapped per backend:
   - **OpenAI:** an assistant `tool_calls` message, then one `role: tool` message per result.
   - **Anthropic:** `tool_use` blocks in the assistant turn, then `tool_result` blocks in a following user turn.

### Permissions and the approval hook

- **Rules per workspace**, stored in `workspaces/<id>/permissions.json`. A rule is `tool` plus an optional pattern; for `bash`, a command prefix such as `cargo test`. Each rule is `allow`, `ask` or `deny`. Defaults:
  - `read`, `list`, `glob`, `grep`: **allow**.
  - `edit`, `write`: **allow**. They only touch the overlay, and the review panel is the gate.
  - `bash`: **ask**. It's sandboxed, but it can use the network and burn time.
  - The plan profile denies `edit`, `write` and `bash`.
- **An `Approver` trait in the core**, `decide(request) -> Decision`, where the request carries the tool, the arguments, the workspace and the conversation.
  - The engine's built-in approver applies the rules. On `ask`, it emits `Event::ApprovalNeeded` and waits for `Command::Approve { id, decision: Once | AlwaysForWorkspace(prefix) | Deny }`.
  - The future auditor is another `Approver` that answers `ask` by calling a model. **That is the slot for auto mode.**
- **The UI** shows a pending approval as a card in the chat: the command, the working directory, and three buttons.

### Context and compaction

- **System prompt**, in order:
  1. The base prompt.
  2. The profile's prompt.
  3. Environment details: the workspace path, OS, date, whether `bash` is available and whether it's sandboxed.
  4. `AGENTS.md`, or failing that `CLAUDE.md`, from the workspace root, through the merged view, capped at 32 KB.
- **Compaction.**
  - **When:** before a request, if the estimated context exceeds `context_limit − buffer`. The buffer defaults to `min(123048, context_limit / 2)`, matching opencode's setting for Strata.
  - **How:**
    1. Ask the same model, without tools, to summarise the conversation so far: goals, decisions, files touched, open threads.
    2. Store the summary as a `Part::Summary` marker message.
    3. Later requests send the system prompt, the summary and the messages after the marker.
  - **What's kept:** the full history stays in the file and in the UI. The chat shows a divider: "Earlier messages summarised".
  - A manual "Compact now" action exists too.

### UI (void-gui)

- **Review panel** (a new dock tab, `Tab::Review`):
  - Conversations with pending changes, each with a count, plus a toggle for "this chat only" versus all chats.
  - A file list with A / M / D badges.
  - A unified diff for the selected file, with buttons to accept or reject a hunk, accept or reject the file, and accept all or reject all.
  - Conflicts shown in the error colour, with "Overwrite" and "Reject".
  - It opens automatically the first time a chat gets pending changes.
- **Tool calls in the chat:** compact cards showing an icon, the tool name, a one-line summary of the arguments, and a status (running / done / denied / error), with the output collapsed. `bash` cards stream their output.
- **Approval cards** for `ask` decisions, inline in the chat.
- **Agent profile picker** in the input card (build / plan), next to the backend picker.
- **Status bar:** a pending-changes pill (for example "7 pending") that opens Review, and a sandbox status pill (shield icon, with a warning colour if bwrap is unavailable).

## Calls made after the builder review (the user can override)

- **Sockets and other ways out of the sandbox** (closes a hole the review found: a read-only mount doesn't stop `connect()`):
  - Also hide `/run/user/$UID`. That covers the D-Bus session bus, through which `systemd-run --user` could run anything unsandboxed, plus the Wayland, ssh-agent, gpg-agent and keyring sockets.
  - Also hide `/run/dbus`, `/var/run/docker.sock` and `/run/docker.sock`, plus `/run/podman` if present.
  - **Landlock is the second layer.** Inside bwrap, a small helper mode of the `void-ai` binary (`void-ai --sandbox-exec -- …`) restricts the process before it runs `bash`:
    - writes are limited to the workspace overlay and `/tmp` (plus the cache and target directories below);
    - abstract Unix sockets and signals are scoped to the sandbox (Landlock ABI ≥ 6, which the 7.3 kernel has).
    - If the kernel's ABI is too old, it runs with bwrap alone and the sandbox pill says so.
  - **Accepted consequence:** in the sandbox, the agent can't use docker/podman, the keyring, gh's stored login, or git over SSH. Pushing and publishing stay with the user. The hidden list is configurable, with an "expose" list for deliberate exceptions.
- **`.git`:** the workspace's real `.git` is bind-mounted **read-only** over the overlay (`--ro-bind <ws>/.git <ws>/.git`).
  - The agent can run `git status`, `git diff` and `git log`. Commands that write fail, and the agent's prompt says so.
  - `.git` never shows up in review. Committing is the user's job after they accept.
- **File tools** (read, list, glob, grep, edit, write) **refuse any path outside the workspace**, including symlinks whose target is outside, and they honour the hidden list. This replaces both the milestone 1 "ask first" rule and the milestone 2 "allowed for read" rule.
- **Build output:**
  - In the sandbox, `CARGO_TARGET_DIR` points to a read-write **per-workspace** `sandbox-target/` under the void.ai data directory. It's shared by that workspace's chats (cargo's lock serialises builds) and stays warm across chats.
  - Other ignored paths (such as `node_modules`) stay in the chat's overlay, hidden from review and with no `base` snapshot.
  - Ignored files that `write` or `edit` created on purpose **do** appear in review, marked "ignored".
  - After "accept all", the overlay keeps the hidden ignored files so the next build stays warm. Discard or delete clears them.
- **Retry and Stop:**
  - At the start of each agent turn, the upper directory is checkpointed (`cp -a --reflink=auto` of `upper/` and `base/`). **Retry restores the checkpoint**, so the abandoned attempt's changes disappear with its messages.
  - **Stop keeps** the changes made so far; they stay in review.
  - Only the latest turn's checkpoint is kept.

- **Package caches: a shared cache overlay** (chosen by the user).
  - **What's covered:** a configurable list of cache directories, mounted in the sandbox as overlays whose writes go to void.ai's own layer, never to the real directories. The defaults, for those that exist:
    - `~/.cargo/registry`, `~/.cargo/git`
    - `~/.npm`, `~/.cache/pip`, `~/.cache/uv`
    - `~/.proto/tools`, `~/.rustup/toolchains`
  - **Storage:** one shared layer per cache, `~/.local/share/void.ai/cache-overlay/<name>/{upper,work}`, kept across chats and workspaces. Landlock allows writes there.
  - **Concurrency:** overlayfs refuses an upper directory that's already in use by another mount. So sandboxed commands from different chats take a **global lock around the cache overlays**, and a second command waits. The status shows "waiting for another chat's command".
    - If that proves too slow, a later change can give each chat its own cache layer at the cost of downloading again.
  - **Clearing:** "Clear sandbox caches" in Settings deletes the cache layers. The real caches are never touched.

## Left to the builder

- Crate choices: `similar` for diffs, `ignore` for gitignore-aware walking, `globset`, `regex`, `tokio::process`.
- The exact wording of system prompts and tool descriptions.
- How diffs are laid out in egui. Unified diffs are fine for milestone 2.
- The internal structure of the overlay and sandbox modules.
- The split into tasks and their order. Suggested: overlay → tools → loop → sandbox → permissions → review UI → compaction.

## Open risks

- **Lower changes while bash runs.** If the user edits a workspace file in another editor while a `bash` command runs, overlayfs behaviour is undefined for that file. It's rare and short-lived, because each command mounts afresh. Accept-time conflict detection catches the result.
- **Copy-up cost.** `bash` modifying a large file copies it into upper first. This is acceptable for source trees.
- **Opaque-directory semantics** depend on the userxattr format, which the experiment observed.
