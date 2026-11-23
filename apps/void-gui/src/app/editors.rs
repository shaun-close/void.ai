//! Editor tabs in the app: opening, closing (asking about unsaved edits), Vim's app-level
//! commands (`:e`, `:q`, `:wa`, `:qa`), Ctrl+S, and quitting with unsaved buffers.

use std::collections::HashMap;
use std::path::PathBuf;

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
                    && let Some((_, dirty)) = tabs.iter().find(|(_, k)| self.editors.is_dirty(k))
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
                let keys: Vec<DocKey> = self
                    .editors
                    .keys()
                    .filter(|k| k.1 == EditorView::Disk)
                    .cloned()
                    .collect();
                for k in keys {
                    if self.editors.is_dirty(&k) {
                        let _ = self.editors.save(&k);
                    }
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

    /// Quitting with unsaved buffers asks first: Save all / Quit without saving / Cancel.
    pub(super) fn quit_dialog(&mut self, ctx: &egui::Context) {
        let dirty: Vec<DocKey> = self
            .editors
            .keys()
            .filter(|k| self.editors.is_dirty(k))
            .cloned()
            .collect();
        if ctx.input(|i| i.viewport().close_requested()) && !self.quit_anyway && !dirty.is_empty() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.quit_dialog = true;
        }
        if !self.quit_dialog {
            return;
        }
        let names: Vec<String> = dirty.iter().map(|k| file_name(&k.0)).collect();
        let mut choice = None;
        let modal = egui::Modal::new(egui::Id::unique("editor-quit")).show(ctx, |ui| {
            ui.set_max_width(420.0);
            theme::heading(ui, icon::FLOPPY, "Save changes before quitting?");
            ui.label(format!("Unsaved: {}", names.join(", ")));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Save all").clicked() {
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
        let quit = match choice {
            Some("save") => dirty.iter().all(|k| self.editors.save(k).is_ok()),
            Some("quit") => true,
            Some(_) => {
                self.quit_dialog = false;
                false
            }
            None => false,
        };
        if quit {
            self.quit_dialog = false;
            self.quit_anyway = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if choice == Some("save") {
            // A save failed: the buffer says why; stay open.
            self.quit_dialog = false;
        }
    }

    /// Save / Discard / Cancel for closing the last tab on a dirty buffer.
    pub(super) fn close_dialog(&mut self, ctx: &egui::Context) {
        let Some(req) = self.editors.confirm.clone() else {
            return;
        };
        let name = file_name(&req.key.0);
        let mut choice = None;
        let modal = egui::Modal::new(egui::Id::unique("editor-close")).show(ctx, |ui| {
            ui.set_max_width(380.0);
            theme::heading(ui, icon::FLOPPY, &format!("Save changes to {name}?"));
            ui.label("Your changes will be lost if you don't save them.");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if theme::primary_button(ui, icon::FLOPPY, "").clicked()
                    || ui.button("Save").clicked()
                {
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
                    self.remove_editor_tab(req.tab);
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
