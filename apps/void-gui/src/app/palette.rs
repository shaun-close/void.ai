//! The palette in the app: its keys (Shift+Shift, Ctrl+Shift+P), the lists it searches, what
//! picking an item does, and the menu commands it shares with the menu bar.

use std::time::Instant;

use void_core::engine::Command;

use super::VoidApp;
use crate::commands::{self, CommandId};
use crate::layout::{self, EditorView, Tab};
use crate::palette::sources::{self, Candidates, Lists};
use crate::palette::{Item, Pick};
use crate::theme;

impl VoidApp {
    /// Open the palette on Shift+Shift or Ctrl+Shift+P, keep its lists fresh, and keep the
    /// editors' hands off the keyboard while it's up. Runs before anything else reads input.
    ///
    /// Shift+Shift reads this frame's events without taking any (an editor with focus still
    /// gets them all, Shift+letter included). It reads them here rather than in
    /// `raw_input_hook` so the headless tests, which don't run that hook, go the same way.
    pub(super) fn palette_keys(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        let shift = &mut self.shift;
        let tapped = ctx.input(|i| {
            i.events
                .iter()
                .fold(false, |hit, e| shift.feed(e, now) | hit)
        });
        // App-wide, taken before an editor could see it.
        let shortcut = ctx.input_mut(|i| {
            i.consume_key(egui::Modifiers::CTRL | egui::Modifiers::SHIFT, egui::Key::P)
        });
        let dialog = self.editors.modal_open() || ctx.memory(|m| m.top_modal_layer()).is_some();
        if (tapped || shortcut) && !self.palette.is_open() && !dialog {
            let lists = self.palette_lists();
            self.palette.open(lists);
        }
        self.palette.files.poll(ctx, now, self.palette.is_open());
        if self.palette.stale() {
            let lists = self.palette_lists();
            self.palette.refresh(lists);
        }
        // Like a dialog: the editors get no keys while it's up (Esc, Enter and typing are the
        // palette's), including on the frame it closes.
        self.editors.dialog |= self.palette.is_open();
    }

    /// Draw the palette and do what was picked. Before the panels, so the keys it takes
    /// (Esc, Enter, arrows) never reach them.
    pub(super) fn palette_ui(&mut self, ctx: &egui::Context) {
        let pal = theme::palette(self.state.config.theme);
        if let Some(pick) = self.palette.show(ctx, &pal) {
            self.palette_pick(pick, ctx);
        }
    }

    fn palette_lists(&self) -> Candidates {
        let tabs = self
            .dock
            .iter_all_tabs()
            .filter_map(|(_, t)| match t {
                Tab::Editor { id, path, view } => {
                    Some((*id, path.clone(), *view != EditorView::Disk))
                }
                _ => None,
            })
            .collect();
        let symbols = self
            .last_editor
            .and_then(|id| self.editor_key(id))
            .and_then(|key| self.editors.doc(&key))
            .map(|d| d.core.syntax.outline(d.core.buffer.rope()))
            .unwrap_or_default();
        sources::collect(&Lists {
            root: self.state.workspace.root.as_deref(),
            files: self.palette.files.files(),
            recent: self.palette.recent.paths(),
            tabs,
            tasks: &self.state.tasks,
            runs: &self.state.runs,
            envs: &self.state.envs,
            symbols,
        })
    }

    fn palette_pick(&mut self, pick: Pick, ctx: &egui::Context) {
        let Pick { item, new_tab } = pick;
        match item {
            Item::File(path) => {
                let path = match &self.state.workspace.root {
                    Some(root) if path.is_relative() => root.join(path),
                    _ => path,
                };
                if new_tab {
                    self.open_editor_tab(path, EditorView::Disk);
                } else {
                    self.open_editor(path, EditorView::Disk, None);
                }
            }
            Item::Tab(t) => {
                if new_tab {
                    self.apply_action(crate::panels::Action::DuplicateEditor(t.id));
                } else {
                    self.focus_editor_tab(t.id);
                }
            }
            Item::Command(c) => self.run_command(c, ctx),
            Item::Task(target) => self.engine.send(Command::RunTask {
                target,
                args: Vec::new(),
            }),
            Item::RunConfig { id, .. } => self.engine.send(Command::RunConfig { config_id: id }),
            Item::Env(name) => self.engine.send(Command::SetActiveEnv(Some(name))),
            // Outline lines are zero-based; editor lines (as in `file:line`) one-based.
            Item::Symbol { line, .. } => {
                if let Some((path, view)) = self.last_editor.and_then(|id| self.editor_key(id)) {
                    self.open_editor(path, view, Some(line + 1));
                }
            }
        }
    }

    /// A menu-bar entry from the command table.
    pub(super) fn command_button(&mut self, ui: &mut egui::Ui, id: CommandId) {
        let d = commands::def(id);
        let mut button = egui::Button::new(theme::itext(d.icon, d.label));
        if let Some(keys) = d.keys {
            button = button.shortcut_text(keys);
        }
        if ui.add(button).clicked() {
            self.run_command(id, ui.ctx());
        }
    }

    /// Do a menu command (from the menu bar or the palette).
    pub(crate) fn run_command(&mut self, id: CommandId, ctx: &egui::Context) {
        let dock = &mut self.dock;
        match id {
            CommandId::OpenFolder => {
                let tx = self.folders.0.clone();
                let ctx = ctx.clone();
                std::thread::spawn(move || {
                    if let Some(p) = rfd::FileDialog::new().pick_folder() {
                        let _ = tx.send(p);
                        ctx.request_repaint();
                    }
                });
            }
            CommandId::Scratch => self.open_workspace(None),
            CommandId::NewChat => self.engine.send(Command::NewConversation),
            CommandId::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            CommandId::NewChatTab => {
                let id = layout::next_chat_id(dock);
                layout::ensure_tab(
                    dock,
                    Tab::Chat {
                        id,
                        conversation: None,
                    },
                );
                self.focused_chat = Some(id);
            }
            CommandId::ResetLayout => {
                *dock = layout::default_layout();
                self.focused_chat = None;
            }
            CommandId::RerunLast => self.engine.send(Command::RerunLast),
            CommandId::ShowChats => layout::ensure_tab(dock, Tab::Conversations),
            CommandId::ShowFiles => layout::ensure_tab(dock, Tab::Files),
            CommandId::ShowRuns => layout::ensure_tab(dock, Tab::Runs),
            CommandId::ShowBackends => layout::ensure_tab(dock, Tab::Backends),
            CommandId::ShowSettings => layout::ensure_tab(dock, Tab::Settings),
            CommandId::ShowReview => layout::ensure_tab(dock, Tab::Review),
        }
    }
}
