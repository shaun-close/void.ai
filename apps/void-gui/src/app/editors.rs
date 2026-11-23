//! Editor tabs in the app: opening, closing (asking about unsaved edits), Vim's app-level
//! commands (`:e`, `:q`, `:wa`, `:qa`), Ctrl+S, and quitting with unsaved buffers.

use std::collections::HashMap;
use std::path::PathBuf;

use void_core::model::ConversationId;

use super::VoidApp;
use crate::editor::state::Msg;
use crate::editor::{CloseRequest, DocKey, EditorAction, file_name};
use crate::layout::{self, EditorView, Tab};
use crate::theme::{self, icon};

impl VoidApp {
    /// Open `path` in an editor tab (focusing one that already shows it), optionally at `line`
    /// (1-based, as in `file:line`). The entry point for the file tree, the palette, `file:line` links and
    /// Review. A file that can't be read opens a tab that says why.
    pub fn open_editor(&mut self, path: PathBuf, view: EditorView, line: Option<usize>) {
        let path = self.absolute(path);
        let key: DocKey = (path.clone(), view.clone());
        let _ = self.editors.open(&key);
        self.show_editor(key, line);
    }

    /// Like [`VoidApp::open_editor`], but a missing file becomes a new buffer (Vim's `:e`).
    fn open_or_create_editor(&mut self, path: PathBuf) {
        let key: DocKey = (self.absolute(path), EditorView::Disk);
        let _ = self.editors.open_or_create(&key);
        self.show_editor(key, None);
    }

    fn show_editor(&mut self, key: DocKey, line: Option<usize>) {
        if let (Some(line), Some(doc)) = (line, self.editors.doc_mut(&key)) {
            doc.goto_line(line.saturating_sub(1));
        }
        let existing = self.dock.iter_all_tabs().find_map(|(_, t)| match t {
            Tab::Editor { id, path, view } if *path == key.0 && *view == key.1 => Some(*id),
            _ => None,
        });
        match existing {
            Some(id) => self.focus_editor_tab(id),
            None => {
                self.open_editor_tab(key.0, key.1);
            }
        }
    }

    /// A new tab on `path` (sharing the buffer with any other tab on it). Returns its id.
    pub fn open_editor_tab(&mut self, path: PathBuf, view: EditorView) -> u32 {
        let id = layout::next_editor_id(&self.dock);
        // Next to the other editors, else where the chats are.
        let near = self
            .dock
            .find_tab_from(|t| matches!(t, Tab::Editor { .. }))
            .or_else(|| self.dock.find_tab_from(|t| matches!(t, Tab::Chat { .. })));
        if let Some(at) = near {
            self.dock.set_focused_node_and_surface(at.node_path());
        }
        self.dock
            .push_to_focused_leaf(Tab::Editor { id, path, view });
        self.editors.focus = Some(id);
        id
    }

    pub fn focus_editor_tab(&mut self, id: u32) {
        if let Some(path) = self.editor_tab_path(id) {
            let _ = self.dock.set_active_tab(path);
            self.dock.set_focused_node_and_surface(path.node_path());
            self.editors.focus = Some(id);
        }
    }

    /// Close an editor tab as its close button does: the last tab on a dirty buffer asks first.
    pub fn close_editor_tab(&mut self, id: u32) {
        let Some(key) = self.editor_key(id) else {
            return;
        };
        let n = self.editor_tab_counts().get(&key).copied().unwrap_or(0);
        if self.editors.needs_confirm(&key, n) {
            self.editors.confirm = Some(CloseRequest { tab: id, key });
        } else {
            self.remove_editor_tab(id);
        }
    }

    fn remove_editor_tab(&mut self, id: u32) {
        if let Some(path) = self.editor_tab_path(id) {
            self.dock.remove_tab(path);
        }
    }

    fn editor_tab_path(&self, id: u32) -> Option<egui_dock::TabPath> {
        self.dock
            .find_tab_from(|t| matches!(t, Tab::Editor { id: i, .. } if *i == id))
    }

    pub(super) fn editor_key(&self, id: u32) -> Option<DocKey> {
        self.dock.iter_all_tabs().find_map(|(_, t)| match t {
            Tab::Editor { id: i, path, view } if *i == id => Some((path.clone(), view.clone())),
            _ => None,
        })
    }

    pub(super) fn editor_tab_counts(&self) -> HashMap<DocKey, usize> {
        let mut n = HashMap::new();
        for (_, t) in self.dock.iter_all_tabs() {
            if let Tab::Editor { path, view, .. } = t {
                *n.entry((path.clone(), view.clone())).or_insert(0) += 1;
            }
        }
        n
    }

    /// Relative paths are relative to the workspace folder.
    fn absolute(&self, path: PathBuf) -> PathBuf {
        match (&self.state.workspace.root, path.is_relative()) {
            (Some(root), true) => root.join(path),
            _ => path,
        }
    }

    fn editor_message(&mut self, key: &DocKey, msg: Msg) {
        if let Some(d) = self.editors.doc_mut(key) {
            d.core.message = Some(msg);
        }
    }

    pub(super) fn apply_editor_action(&mut self, action: EditorAction) {
        const E37: &str = "E37: No write since last change (add ! to override)";
        match action {
            EditorAction::Open { path, from } => {
                let path = if path.is_relative() && self.state.workspace.root.is_none() {
                    from.parent().map_or(path.clone(), |d| d.join(&path))
                } else {
                    path
                };
                self.open_or_create_editor(path);
            }
            EditorAction::Close { tab, force } => {
                let Some(key) = self.editor_key(tab) else {
                    return;
                };
                let n = self.editor_tab_counts().get(&key).copied().unwrap_or(0);
                if !force && self.editors.needs_confirm(&key, n) {
                    // `:wq` in a chat view: the save is on its way; close when it lands.
                    if let Some(request) = self.editors.saving(&key) {
                        self.closing.push((tab, request));
                        return;
                    }
                    self.editor_message(&key, Msg::error(E37));
                    return;
                }
                self.remove_editor_tab(tab);
                if n <= 1 {
                    self.editors.discard(&key);
                }
            }
            EditorAction::CloseAll { force } => {
                let tabs: Vec<(u32, DocKey)> = self
                    .dock
                    .iter_all_tabs()
                    .filter_map(|(_, t)| match t {
                        Tab::Editor { id, path, view } => Some((*id, (path.clone(), view.clone()))),
                        _ => None,
                    })
                    .collect();
                if !force
                    && let Some((_, dirty)) = tabs.iter().find(|(_, k)| self.editors.unsaved(k))
                {
                    let dirty = dirty.clone();
                    let name = file_name(&dirty.0);
                    for (_, k) in &tabs {
                        self.editor_message(
                            k,
                            Msg::error(format!(
                                "E162: No write since last change for buffer \"{name}\""
                            )),
                        );
                    }
                    return;
                }
                for (id, key) in tabs {
                    self.remove_editor_tab(id);
                    self.editors.discard(&key);
                }
            }
            EditorAction::SaveAll => {
                let keys: Vec<DocKey> = self.editors.keys().cloned().collect();
                for k in keys {
                    if self.editors.is_dirty(&k) {
                        let _ = self.editors.save(&k);
                    }
                }
            }
            EditorAction::Save { tab, force } => {
                if let Some(key) = self.editor_key(tab) {
                    let _ = self.editors.save_chat(&key, force);
                }
            }
            EditorAction::Reload { tab, force } => {
                let Some(key) = self.editor_key(tab) else {
                    return;
                };
                if !force && self.editors.is_dirty(&key) {
                    self.editor_message(&key, Msg::error(E37));
                    return;
                }
                let msg = match self.editors.reload(&key) {
                    Ok(()) => Msg::info(format!("\"{}\" reloaded", file_name(&key.0))),
                    Err(e) => Msg::error(e),
                };
                self.editor_message(&key, msg);
            }
        }
    }

    /// Show another version of a tab's file. The old view's unsaved edits stay in their
    /// buffer (switching back brings them back; quitting asks about them).
    pub(super) fn switch_editor_view(&mut self, tab: u32, view: EditorView) {
        let Some(old) = self.editor_key(tab) else {
            return;
        };
        if old.1 == view {
            return;
        }
        let dirty = self.editors.is_dirty(&old);
        for (_, t) in self.dock.iter_all_tabs_mut() {
            if let Tab::Editor { id, view: v, .. } = t
                && *id == tab
            {
                *v = view.clone();
            }
        }
        let key: DocKey = (old.0.clone(), view);
        let _ = self.editors.open(&key);
        if dirty {
            let from = match old.1 {
                EditorView::Disk => "the Disk view".to_string(),
                EditorView::Chat(c) => format!("void's version in {}", self.state.chat_title(c)),
            };
            self.editor_message(
                &key,
                Msg::info(format!(
                    "Unsaved edits in {from} are kept: switch back to save them"
                )),
            );
        }
        self.focus_editor_tab(tab);
    }

    /// Chats an editor tab can show the version of: those with pending changes (listed when
    /// the workspace opened) or open in a Chat tab, in the list's order.
    pub(super) fn view_chats(&self) -> Vec<ConversationId> {
        if self.state.workspace.root.is_none() {
            return Vec::new();
        }
        let open: Vec<ConversationId> = self
            .dock
            .iter_all_tabs()
            .filter_map(|(_, t)| match t {
                Tab::Chat {
                    conversation: Some(c),
                    ..
                } => Some(*c),
                _ => None,
            })
            .collect();
        self.state
            .conversations
            .iter()
            .map(|m| m.id)
            .filter(|id| self.state.changes.contains_key(id) || open.contains(id))
            .collect()
    }

    /// Engine news for chat views: saves that landed or failed (tabs waiting to close close,
    /// or stay with the error), chats whose changes moved.
    pub(super) fn chat_view_events(&mut self) {
        for r in self.state.take_overlay_saves() {
            match &r.error {
                None => self
                    .editors
                    .overlay_saved(r.request, r.conversation, &r.path),
                Some(e) => self
                    .editors
                    .overlay_save_failed(r.request, r.conversation, &r.path, e),
            }
            self.chat_results.insert(r.request, r.error);
        }
        for (tab, request) in std::mem::take(&mut self.closing) {
            match self.chat_results.get(&request) {
                None => self.closing.push((tab, request)),
                Some(Some(_)) => {}
                Some(None) => {
                    let Some(key) = self.editor_key(tab) else {
                        continue;
                    };
                    if self.editors.is_dirty(&key) {
                        continue;
                    }
                    let n = self.editor_tab_counts().get(&key).copied().unwrap_or(0);
                    self.remove_editor_tab(tab);
                    if n <= 1 {
                        self.editors.discard(&key);
                    }
                }
            }
        }
        let waiting: Vec<u64> = self.quit_saving.clone();
        self.chat_results.retain(|r, _| waiting.contains(r));
        let changed = self.state.take_changed_chats();
        if !changed.is_empty() {
            self.editors.check_overlays(&changed);
        }
    }

    /// Send the chat-view saves queued this frame.
    pub(super) fn send_overlay_saves(&mut self) {
        for s in std::mem::take(&mut self.editors.overlay_saves) {
            self.engine.send(void_core::engine::Command::SaveToOverlay {
                request: s.request,
                conversation: s.conversation,
                path: s.path,
                content: s.content,
            });
        }
    }

    /// Ctrl+S saves the focused editor tab, wherever the keyboard focus is.
    pub(super) fn global_save(&mut self, ctx: &egui::Context) {
        let key = match self.dock.find_active_focused() {
            Some((_, Tab::Editor { path, view, .. })) => (path.clone(), view.clone()),
            _ => return,
        };
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::S)) {
            let _ = self.editors.save(&key);
        }
    }

    /// Quitting with unsaved buffers (edits, or files deleted on disk) asks first: Save all /
    /// Quit without saving / Cancel. Chat-view saves go through the engine: the dialog waits
    /// for them ("Saving…") and quits only when every save landed; otherwise it stays up with
    /// the errors.
    pub(super) fn quit_dialog(&mut self, ctx: &egui::Context) {
        let dirty: Vec<DocKey> = self
            .editors
            .keys()
            .filter(|k| self.editors.unsaved(k))
            .cloned()
            .collect();
        if ctx.input(|i| i.viewport().close_requested()) && !self.quit_anyway && !dirty.is_empty() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.quit_dialog = true;
        }
        if !self.quit_dialog {
            return;
        }
        let mut quit = false;
        if !self.quit_saving.is_empty() {
            let answers: Vec<Option<String>> = self
                .quit_saving
                .iter()
                .filter_map(|r| self.chat_results.get(r).cloned())
                .collect();
            if answers.len() == self.quit_saving.len() {
                self.quit_saving.clear();
                self.quit_errors = answers.into_iter().flatten().collect();
                quit = self.quit_errors.is_empty();
            }
        }
        let saving = !self.quit_saving.is_empty();
        let names: Vec<String> = dirty
            .iter()
            .map(|k| {
                if self.editors.is_dirty(k) {
                    file_name(&k.0)
                } else {
                    format!("{} (deleted on disk)", file_name(&k.0))
                }
            })
            .collect();
        let errors = self.quit_errors.clone();
        let mut choice = None;
        let modal = egui::Modal::new(egui::Id::unique("editor-quit")).show(ctx, |ui| {
            let pal = theme::palette_of(ui);
            ui.set_max_width(420.0);
            theme::heading(ui, icon::FLOPPY, "Save changes before quitting?");
            ui.label(format!("Unsaved: {}", names.join(", ")));
            for e in &errors {
                ui.label(theme::itext_styled(
                    icon::WARNING,
                    e,
                    theme::small_size(),
                    pal.err,
                ));
            }
            ui.add_space(8.0);
            if saving {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Saving to void's pending changes…");
                });
            } else {
                ui.label(
                    egui::RichText::new("Enter saves all, Esc cancels")
                        .small()
                        .color(pal.text_faint),
                );
                if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)) {
                    choice = Some("save");
                }
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!saving, egui::Button::new("Save all"))
                    .clicked()
                {
                    choice = Some("save");
                }
                if ui.button("Quit without saving").clicked() {
                    choice = Some("quit");
                }
                if ui.button("Cancel").clicked() {
                    choice = Some("cancel");
                }
            });
        });
        if modal.should_close() && choice.is_none() {
            choice = Some("cancel");
        }
        match choice {
            Some("save") => {
                self.quit_errors.clear();
                let mut pending = Vec::new();
                for k in &dirty {
                    match self.editors.save(k) {
                        Ok(_) => pending.extend(self.editors.saving(k)),
                        Err(e) => self.quit_errors.push(e),
                    }
                }
                if self.quit_errors.is_empty() {
                    // Disk saves are done; chat-view saves answer in a later frame.
                    quit = pending.is_empty();
                    self.quit_saving = pending;
                }
            }
            Some("quit") => quit = true,
            Some(_) => {
                self.quit_dialog = false;
                self.quit_saving.clear();
                self.quit_errors.clear();
            }
            None => {}
        }
        if quit {
            self.quit_dialog = false;
            self.quit_anyway = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// Save / Discard / Cancel for closing the last tab on a dirty buffer.
    pub(super) fn close_dialog(&mut self, ctx: &egui::Context) {
        let Some(req) = self.editors.confirm.clone() else {
            return;
        };
        let name = file_name(&req.key.0);
        let deleted = self.editors.is_deleted(&req.key) && !self.editors.is_dirty(&req.key);
        let mut choice = None;
        let modal = egui::Modal::new(egui::Id::unique("editor-close")).show(ctx, |ui| {
            ui.set_max_width(380.0);
            if deleted {
                theme::heading(ui, icon::FLOPPY, &format!("Save {name}?"));
                ui.label("It was deleted on disk; this tab holds the only copy.");
            } else {
                theme::heading(ui, icon::FLOPPY, &format!("Save changes to {name}?"));
                ui.label("Your changes will be lost if you don't save them.");
            }
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("Enter saves, Esc cancels")
                    .small()
                    .color(theme::palette_of(ui).text_faint),
            );
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)) {
                choice = Some("save");
            }
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    choice = Some("save");
                }
                if ui.button("Discard").clicked() {
                    choice = Some("discard");
                }
                if ui.button("Cancel").clicked() {
                    choice = Some("cancel");
                }
            });
        });
        if modal.should_close() && choice.is_none() {
            choice = Some("cancel");
        }
        match choice {
            Some("save") => {
                self.editors.confirm = None;
                if self.editors.save(&req.key).is_ok() {
                    // A chat view's save answers later: close when it lands.
                    match self.editors.saving(&req.key) {
                        Some(request) => self.closing.push((req.tab, request)),
                        None => self.remove_editor_tab(req.tab),
                    }
                }
            }
            Some("discard") => {
                self.editors.confirm = None;
                self.remove_editor_tab(req.tab);
                self.editors.discard(&req.key);
            }
            Some(_) => self.editors.confirm = None,
            None => {}
        }
    }
}
