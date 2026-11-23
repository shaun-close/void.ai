//! The engine's side of Claude Code chats: starting a turn's task, applying what it reports,
//! Stop, and the checks that keep a running turn's overlay mount safe. The process and the
//! stream mapping live in [`crate::claude_code::turn`].

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::{Event, Msg, State, StreamMode};
use crate::claude_code::permission::{self as perm, HostMsg};
use crate::claude_code::turn::{
    self as cc, Out, PermissionAsk, STOPPED_TOOL, Segments, StdinTx, TurnSpec, TurnUpdate,
};
use crate::claude_code::{SessionMode, TurnArgs, control_response_deny};
use crate::config::{BackendKind, Profile};
use crate::model::{Conversation, ConversationId, Message, Part, Role, WorkspaceId};
use crate::sandbox::{Availability, OverlayMounts, Sandbox};
use crate::tools::ToolOutput;

/// A running Claude Code turn (alongside its `Turn`, which keeps the chat busy until the
/// process is gone, so its overlay is never mounted twice).
pub(super) struct ClaudeRun {
    pub(super) generation: u64,
    pub(super) cancel: CancellationToken,
    pub(super) seg: Segments,
    /// The reply is over (result, failure, Stop or Delete); later stream content is ignored.
    pub(super) done: bool,
    /// Where the turn has its overlay mounted (kept after Delete until the process is gone).
    pub(super) workspace: WorkspaceId,
    /// The current attempt's `claude` stdin: permission requests from any other are stale.
    pub(super) stdin: Option<StdinTx>,
    /// Claude Code's pending permission requests: request id → approval id.
    pub(super) asks: HashMap<String, u64>,
    /// `claude` started the session (so it has this turn's input): the chat's `claude_upto`
    /// moves to the end of the turn when it's over.
    pub(super) seen: bool,
}

impl State {
    /// The conversation's effective profile.
    pub(super) fn profile_of(&self, c: &Conversation) -> Option<&Profile> {
        let id = c
            .profile_id
            .as_deref()
            .unwrap_or(&self.config.active_profile);
        self.config.profile(id)
    }

    pub(super) fn is_claude(&self, c: &Conversation) -> bool {
        self.profile_of(c)
            .is_some_and(|p| p.kind == BackendKind::ClaudeCode)
    }

    /// Whether a Claude Code turn is running in this workspace.
    pub(super) fn claude_running_in(&self, ws: &WorkspaceId) -> bool {
        self.claude.values().any(|run| run.workspace == *ws)
    }

    /// Start the reply to a turn just begun: through Claude Code or void's own agent loop.
    pub(super) fn reply(&mut self, c: Conversation, retry: bool) {
        if self.is_claude(&c) {
            self.start_claude(c, retry);
        } else {
            self.start_stream(c, StreamMode::Reply);
        }
    }

    /// Run the turn through `claude` (the turn is already begun).
    fn start_claude(&mut self, c: Conversation, retry: bool) {
        let id = c.id;
        let generation = self.next_gen;
        self.next_gen += 1;
        let cancel = CancellationToken::new();
        let mut seg = Segments::default();
        let outs = seg.start();
        self.claude.insert(
            id,
            ClaudeRun {
                generation,
                cancel: cancel.clone(),
                seg,
                done: false,
                workspace: self
                    .turns
                    .get(&id)
                    .map(|t| t.workspace.id.clone())
                    .unwrap_or_else(|| self.workspace.id.clone()),
                stdin: None,
                asks: HashMap::new(),
                seen: false,
            },
        );
        self.apply_outs(id, outs);
        let (sandbox, spec) = match self.claude_spec(&c, retry) {
            Ok(x) => x,
            Err(error) => {
                let update = TurnUpdate::Exited { error: Some(error) };
                self.claude_update(id, generation, update);
                return;
            }
        };
        let tx = self.tx.clone();
        let bridge = self.claude_bridge.clone();
        tokio::spawn(async move {
            // If the task dies without reporting the end (a panic), the backstop does, so the
            // chat doesn't stay busy for good.
            let backstop = cc::Backstop::new(move |update| {
                let _ = tx.send(Msg::Claude {
                    conversation: id,
                    generation,
                    update,
                });
            });
            let send = |update| backstop.send(update);
            cc::run_turn(sandbox, spec, bridge, cancel, &send).await;
        });
    }

    /// Everything a Claude Code turn needs, or why it can't run.
    fn claude_spec(
        &self,
        c: &Conversation,
        retry: bool,
    ) -> Result<(Arc<Sandbox>, TurnSpec), String> {
        let profile = self
            .profile_of(c)
            .ok_or("There's no Claude Code profile for this chat.")?;
        let program = cc::resolve_command(profile.claude_command(), &self.home)?;
        if !self.config.sandbox.network {
            return Err(
                "Claude Code needs the network, but the sandbox has it turned off \
                        ([sandbox] network = false)."
                    .into(),
            );
        }
        let sandbox = match (&self.sandbox, &self.sandbox_status) {
            (Some(sb), Availability::Ready { .. }) => sb.clone(),
            (_, Availability::Unavailable(why)) => {
                return Err(format!(
                    "Claude Code runs in the sandbox, which isn't available: {why}"
                ));
            }
            (None, _) => return Err("Claude Code runs in the sandbox, which is off.".into()),
        };
        let ws = self
            .turns
            .get(&c.id)
            .map(|t| t.workspace.clone())
            .unwrap_or_else(|| self.workspace.clone());
        let (overlay, cwd) = match &ws.root {
            Some(root) => {
                let ov = self
                    .overlay_for(&ws, c.id)
                    .ok_or("Couldn't open the chat's pending changes.")?;
                let mounts = OverlayMounts {
                    workspace: root.clone(),
                    upper: ov.upper(),
                    work: ov.work(),
                    target_dir: self.store.sandbox_target(&ws.id),
                };
                (Some(mounts), root.clone())
            }
            None => {
                let dir = self
                    .store
                    .data_dir()
                    .join("claude-scratch")
                    .join(c.id.to_string());
                std::fs::create_dir_all(&dir)
                    .map_err(|e| format!("Couldn't prepare {}: {e}", dir.display()))?;
                (None, dir)
            }
        };
        let last = c
            .messages
            .iter()
            .rposition(|m| m.role == Role::User)
            .ok_or("There's no message to answer.")?;
        let message = c.messages[last].text();
        let fresh_text = cc::with_transcript(c, last, &message);
        // A resumed session hasn't seen the turns another model had since its last turn.
        let body = match c.claude_upto {
            Some(seen) if seen < last => cc::with_meanwhile(c, seen, last, &message),
            _ => message,
        };
        let (session, text) = match c.claude_session {
            None => (SessionMode::New(c.id.0), fresh_text.clone()),
            Some(u) if retry => (
                SessionMode::Resume(u),
                format!("{}\n\n{body}", cc::RETRY_NOTE),
            ),
            Some(u) => (SessionMode::Resume(u), body),
        };
        let model = profile.model.trim();
        let args = TurnArgs {
            model: (!model.is_empty()).then(|| model.to_string()),
            plan_mode: self.agent_for(c).is_some_and(|a| a.id == "plan"),
            session,
            append_system: cc::VOID_NOTE.into(),
        };
        let spec = TurnSpec {
            conversation: c.id,
            program,
            args,
            overlay,
            cwd,
            home: self.home.clone(),
            api_key: profile.api_key.clone(),
            text,
            fresh_text,
        };
        Ok((sandbox, spec))
    }

    pub(super) fn claude_update(
        &mut self,
        id: ConversationId,
        generation: u64,
        update: TurnUpdate,
    ) {
        let Some(run) = self.claude.get_mut(&id) else {
            return;
        };
        if run.generation != generation {
            return;
        }
        let status = |this: &Self, status: String| {
            this.emit(Event::TurnStatus {
                conversation: id,
                status,
            })
        };
        match update {
            TurnUpdate::Waiting => status(
                self,
                "Waiting for the sandbox — another chat's command is using it…".into(),
            ),
            TurnUpdate::Spawned(tx) => run.stdin = Some(tx),
            TurnUpdate::Session { session, model } => {
                run.seen = true;
                // Gone from `convs` once deleted: nothing to store (or recreate).
                let Some(c) = self.convs.get_mut(&id) else {
                    return;
                };
                if c.claude_session != Some(session) {
                    c.claude_session = Some(session);
                    let c = c.clone();
                    self.save(&c);
                }
                self.emit(Event::ClaudeInit {
                    conversation: id,
                    model,
                });
            }
            TurnUpdate::Restarting => {
                if !run.done {
                    // The failed attempt's running calls are in no stored message: close
                    // their cards without recording a result.
                    for call_id in run.seg.reset() {
                        self.emit(Event::ToolFinished {
                            conversation: id,
                            call_id,
                            output: ToolOutput {
                                content: STOPPED_TOOL.into(),
                                is_error: true,
                            },
                        });
                    }
                    status(
                        self,
                        "Claude Code didn't have this chat's session — starting a new one with \
                         the transcript…"
                            .into(),
                    );
                }
            }
            TurnUpdate::Status(s) => {
                if !run.done {
                    status(self, s);
                }
            }
            TurnUpdate::Event(ev) => {
                if !run.done {
                    let outs = run.seg.on_event(ev);
                    self.apply_outs(id, outs);
                }
            }
            TurnUpdate::Result { usage, error } => {
                if !run.done {
                    run.done = true;
                    let outs = run.seg.end(false, error, Some(usage));
                    self.apply_outs(id, outs);
                }
                // stdin closes now: nothing can be answered any more.
                self.claude_turn_ended(id);
            }
            TurnUpdate::Exited { error } => {
                let seen = run.seen;
                if !run.done {
                    run.done = true;
                    let error =
                        error.unwrap_or_else(|| "Claude Code ended without a reply.".into());
                    let outs = run.seg.end(false, Some(error), None);
                    self.apply_outs(id, outs);
                }
                // The session has everything up to here (its own turn included).
                if seen
                    && let Some(c) = self.convs.get_mut(&id)
                    && c.claude_upto != Some(c.messages.len())
                {
                    c.claude_upto = Some(c.messages.len());
                    let c = c.clone();
                    self.save(&c);
                }
                self.claude_turn_ended(id);
                self.claude.remove(&id);
                self.finalize_turn(id);
                if self
                    .convs
                    .get(&id)
                    .is_some_and(|c| c.workspace_id != self.workspace.id)
                {
                    self.convs.remove(&id);
                }
            }
        }
    }

    /// Carry out what a Claude Code turn's stream mapped to.
    fn apply_outs(&mut self, id: ConversationId, outs: Vec<Out>) {
        for out in outs {
            match out {
                Out::StreamStarted => self.emit(Event::StreamStarted(id)),
                Out::Delta(kind, text) => self.emit(Event::Delta {
                    conversation: id,
                    kind,
                    text,
                }),
                Out::Finished {
                    message,
                    tokens_per_sec,
                } => match message.error.clone() {
                    Some(error) => {
                        if let Some(c) = self.append(id, message) {
                            self.emit(Event::Conversation(c));
                        }
                        self.emit(Event::StreamFailed {
                            conversation: id,
                            error,
                        });
                    }
                    None => {
                        self.append(id, message.clone());
                        self.emit(Event::StreamFinished {
                            conversation: id,
                            message,
                            tokens_per_sec,
                        });
                        self.emit_list();
                    }
                },
                Out::ToolStarted {
                    call_id,
                    name,
                    summary,
                } => self.emit(Event::ToolStarted {
                    conversation: id,
                    call_id,
                    name,
                    summary,
                }),
                Out::ToolOutput { call_id, text } => self.emit(Event::ToolOutput {
                    conversation: id,
                    call_id,
                    text,
                }),
                Out::ToolFinished {
                    call_id,
                    content,
                    is_error,
                } => {
                    let msg = Message::new(
                        Role::Tool,
                        vec![Part::ToolResult {
                            call_id: call_id.clone(),
                            content: content.clone(),
                        }],
                    );
                    self.append(id, msg);
                    self.emit(Event::ToolFinished {
                        conversation: id,
                        call_id,
                        output: ToolOutput { content, is_error },
                    });
                }
            }
        }
    }

    /// Stop a Claude Code turn: keep the partial reply (stopped), refuse its pending approvals
    /// and kill `claude`. The chat stays busy until the process is gone. False when the chat
    /// has no Claude Code turn.
    pub(super) fn stop_claude(&mut self, id: ConversationId) -> bool {
        let Some(run) = self.claude.get_mut(&id) else {
            return false;
        };
        run.cancel.cancel();
        let outs = if run.done {
            Vec::new()
        } else {
            run.done = true;
            run.seg.end(true, None, None)
        };
        // The bridge hears of the end before anyone sees the reply finish.
        self.claude_turn_ended(id);
        self.apply_outs(id, outs);
        true
    }

    /// The chat's Claude Code turn is over (or stopped): close its permission prompts, unanswered
    /// (no stdin writes after this), and tell the bridge.
    fn claude_turn_ended(&mut self, id: ConversationId) {
        if let Some(run) = self.claude.get_mut(&id) {
            run.asks.clear();
        }
        self.withdraw_approvals(id);
        self.claude_bridge.turn_ended(id);
    }

    pub(super) fn claude_permission(&mut self, m: HostMsg) {
        match m {
            HostMsg::Ask {
                conversation,
                ask,
                stdin,
            } => self.claude_ask(conversation, ask, stdin),
            HostMsg::Cancel {
                conversation,
                request_id,
            } => {
                let k = self
                    .claude
                    .get_mut(&conversation)
                    .and_then(|run| run.asks.remove(&request_id));
                if let Some(k) = k
                    && self.approvals.remove(&k).is_some()
                {
                    self.emit(Event::ApprovalResolved { id: k });
                }
            }
        }
    }

    /// Show Claude Code's permission request in the approval dialog, like void's own requests,
    /// and answer it on `stdin` when the user decides. void's permission rules play no part.
    /// A request from a turn that's over (or from an earlier attempt) is refused at once, so
    /// `claude` never waits for a dialog that won't come.
    fn claude_ask(&mut self, id: ConversationId, ask: PermissionAsk, stdin: StdinTx) {
        let live = self.turns.contains_key(&id)
            && self.claude.get(&id).is_some_and(|run| {
                !run.done
                    && run
                        .stdin
                        .as_ref()
                        .is_some_and(|current| current.same_channel(&stdin))
            });
        if !live {
            let _ = stdin.send(control_response_deny(
                &ask.request_id,
                perm::TURN_OVER,
                false,
            ));
            return;
        }
        let base = self.turns.get(&id).and_then(|t| t.workspace.root.clone());
        let req = perm::approval_request(id, &ask, base.as_deref());
        let (reply, decided) = oneshot::channel();
        let k = self.request_approval(req, reply);
        if let Some(run) = self.claude.get_mut(&id) {
            run.asks.insert(ask.request_id.clone(), k);
        }
        tokio::spawn(async move {
            // No answer when the prompt was withdrawn (cancelled, stopped, turn over).
            if let Ok(decision) = decided.await {
                let _ = stdin.send(perm::answer(&ask, &decision));
            }
        });
    }
}

impl State {
    /// A chat with a Claude Code turn is being deleted: kill `claude`, but keep the run (and so
    /// the workspace's review refusal) until the process is gone.
    pub(super) fn delete_claude(&mut self, id: ConversationId) {
        if let Some(run) = self.claude.get_mut(&id) {
            run.cancel.cancel();
            run.done = true;
            self.claude_turn_ended(id);
        }
    }
}
