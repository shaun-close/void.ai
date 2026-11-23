//! Editor tabs: buffers shared per (path, view), background parsing, the Vim host, drawing and
//! input. The app opens files with `VoidApp::open_editor`; tabs draw through [`show`].

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use egui::{RichText, Ui};
use void_core::config::EditorSettings;
use void_editor::{Lang, Syntax, Tree};

mod chat;
pub mod draw;
mod host;
pub mod input;
mod paint;
pub mod state;
mod status;
mod views;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod view_tests;

pub use chat::{ChatOverlays, OverlaySave};
pub use host::file_name;
pub use state::Doc;

use crate::layout::EditorView;
use crate::panels::Panels;
use crate::theme::{self, icon};

/// One buffer per file and view; tabs with the same key share it.
pub type DocKey = (PathBuf, EditorView);

/// Wait this long after the last edit before re-parsing.
const PARSE_DEBOUNCE: Duration = Duration::from_millis(50);

/// App-level requests from an editor tab (Vim ex commands), applied after the dock is drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorAction {
    /// `:e path` (relative to the workspace, else to the file it was typed in).
    Open { path: PathBuf, from: PathBuf },
    /// `:q` / `:q!`.
    Close { tab: u32, force: bool },
    /// `:qa` / `:qa!`.
    CloseAll { force: bool },
    /// `:wa`.
    SaveAll,
    /// `:e` / `:e!` with no file: reload from disk (or the chat's version).
    Reload { tab: u32, force: bool },
    /// `:w` / `:w!` in a chat view (Disk views save in place).
    Save { tab: u32, force: bool },
}

/// A dirty tab the user asked to close: Save / Discard / Cancel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseRequest {
    pub tab: u32,
    pub key: DocKey,
}

/// The system clipboard (`+`/`*` registers, Ctrl+Shift+C/V), or an in-memory one in tests.
pub enum Clipboard {
    System(RefCell<Option<arboard::Clipboard>>),
    Memory(RefCell<Option<String>>),
}

impl Clipboard {
    pub fn system() -> Clipboard {
        Clipboard::System(RefCell::new(None))
    }

    pub fn memory() -> Clipboard {
        Clipboard::Memory(RefCell::new(None))
    }

    pub fn get(&self) -> Option<String> {
        match self {
            Clipboard::System(c) => {
                let mut c = c.borrow_mut();
                if c.is_none() {
                    *c = arboard::Clipboard::new().ok();
                }
                c.as_mut()?.get_text().ok()
            }
            Clipboard::Memory(m) => m.borrow().clone(),
        }
    }

    pub fn set(&self, text: &str) {
        match self {
            Clipboard::System(c) => {
                let mut c = c.borrow_mut();
                if c.is_none() {
                    *c = arboard::Clipboard::new().ok();
                }
                if let Some(c) = c.as_mut() {
                    let _ = c.set_text(text.to_string());
                }
            }
            Clipboard::Memory(m) => *m.borrow_mut() = Some(text.to_string()),
        }
    }
}

struct Parsed {
    key: DocKey,
    version: u64,
    tree: Option<Tree>,
}

/// Every open buffer, plus what editor tabs share: the clipboard, pending actions, the close
/// dialog and the editor settings.
pub struct EditorStore {
    docs: HashMap<DocKey, Doc>,
    failed: HashMap<DocKey, String>,
    tx: mpsc::Sender<Parsed>,
    rx: mpsc::Receiver<Parsed>,
    pub clipboard: Clipboard,
    pub actions: Vec<EditorAction>,
    pub confirm: Option<CloseRequest>,
    /// The tab whose editor should take keyboard focus when it's next drawn.
    pub focus: Option<u32>,
    pub settings: EditorSettings,
    /// The editor widget that had keyboard focus last frame (clipboard keys go to it).
    pub focused_widget: Option<egui::Id>,
    /// One of the app's own dialogs (quit, delete from the file tree) is up.
    pub dialog: bool,
    /// The folder workspace's chat layers (None in Scratch: no chat views there).
    pub overlays: Option<ChatOverlays>,
    /// Chats whose agent turn is running (their views can't be saved).
    pub busy_chats: std::collections::HashSet<void_core::model::ConversationId>,
    /// Chat-view saves for the app to send to the engine.
    pub overlay_saves: Vec<OverlaySave>,
}

impl EditorStore {
    pub fn new(clipboard: Clipboard) -> EditorStore {
        let (tx, rx) = mpsc::channel();
        EditorStore {
            docs: HashMap::new(),
            failed: HashMap::new(),
            tx,
            rx,
            clipboard,
            actions: Vec::new(),
            confirm: None,
            focus: None,
            settings: EditorSettings::default(),
            focused_widget: None,
            dialog: false,
            overlays: None,
            busy_chats: Default::default(),
            overlay_saves: Vec::new(),
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.docs.len()
    }

    /// A dialog (close, quit, delete) is waiting for the user.
    pub fn modal_open(&self) -> bool {
        self.confirm.is_some() || self.dialog
    }

    pub fn doc(&self, key: &DocKey) -> Option<&Doc> {
        self.docs.get(key)
    }

    pub fn doc_mut(&mut self, key: &DocKey) -> Option<&mut Doc> {
        self.docs.get_mut(key)
    }

    pub fn keys(&self) -> impl Iterator<Item = &DocKey> {
        self.docs.keys()
    }

    /// Why the file couldn't be opened, if it couldn't.
    pub fn failure(&self, key: &DocKey) -> Option<&str> {
        self.failed.get(key).map(String::as_str)
    }

    /// Load the buffer for `key` unless it's open: the file, or a chat's version of it.
    pub fn open(&mut self, key: &DocKey) -> Result<(), String> {
        if self.docs.contains_key(key) {
            return Ok(());
        }
        if key.1 != EditorView::Disk {
            return self.open_chat(key);
        }
        match Doc::open(&key.0) {
            Ok(doc) => {
                self.failed.remove(key);
                self.docs.insert(key.clone(), doc);
                Ok(())
            }
            Err(e) => {
                let msg = format!("Couldn't open {}: {e}", key.0.display());
                self.failed.insert(key.clone(), msg.clone());
                Err(msg)
            }
        }
    }

    /// Like [`EditorStore::open`], but a missing file opens as a new, empty buffer (Vim's `:e`).
    pub fn open_or_create(&mut self, key: &DocKey) -> Result<(), String> {
        if !self.docs.contains_key(key) && key.1 == EditorView::Disk && !key.0.exists() {
            let mut doc = Doc::new(&key.0, void_editor::Buffer::new(""));
            doc.core.new_file = true;
            doc.core.message = Some(state::Msg::info(format!("\"{}\" [New]", file_name(&key.0))));
            self.failed.remove(key);
            self.docs.insert(key.clone(), doc);
            return Ok(());
        }
        self.open(key)
    }

    /// Forget a failed open so the tab tries again.
    pub fn retry(&mut self, key: &DocKey) {
        self.failed.remove(key);
    }

    pub fn is_dirty(&self, key: &DocKey) -> bool {
        self.docs.get(key).is_some_and(|d| d.core.buffer.is_dirty())
    }

    /// Closing the last tab on `key` loses edits, or the only copy of a file deleted on
    /// disk: ask first.
    pub fn needs_confirm(&self, key: &DocKey, tabs_on_key: usize) -> bool {
        tabs_on_key <= 1 && (self.is_dirty(key) || self.is_deleted(key))
    }

    /// The buffer's file was deleted on disk (the buffer holds the last copy).
    pub fn is_deleted(&self, key: &DocKey) -> bool {
        self.docs
            .get(key)
            .is_some_and(|d| d.core.disk == state::DiskState::Deleted)
    }

    /// Save to disk, or (a chat view) to the chat's pending changes through the engine.
    pub fn save(&mut self, key: &DocKey) -> Result<String, String> {
        if key.1 != EditorView::Disk {
            return self.save_chat(key, false);
        }
        let doc = self
            .docs
            .get_mut(key)
            .ok_or_else(|| "That file isn't open".to_string())?;
        let r = doc.core.save(&key.0);
        doc.core.message = Some(match &r {
            Ok(m) => state::Msg::info(m.clone()),
            Err(e) => state::Msg::error(e.clone()),
        });
        r
    }

    /// Reload a buffer from disk (keeps the cursor line; the buffer ends up clean). For
    /// external changes (Task 7) and `:e!`.
    pub fn reload(&mut self, key: &DocKey) -> Result<(), String> {
        if key.1 != EditorView::Disk {
            return self.reload_chat(key);
        }
        let doc = self
            .docs
            .get_mut(key)
            .ok_or_else(|| "That file isn't open".to_string())?;
        doc.reload(&key.0)
            .map_err(|e| format!("Couldn't reload {}: {e}", key.0.display()))
    }

    /// Files changed on disk (watcher events; a folder stands for everything in it): check
    /// the buffers on them. Returns how many were reloaded or flagged.
    pub fn check_disk(&mut self, changed: &[PathBuf]) -> usize {
        let hit = |p: &Path| changed.iter().any(|c| p == c || p.starts_with(c));
        let mut n = 0;
        for (key, doc) in &mut self.docs {
            if key.1 == EditorView::Disk
                && hit(&key.0)
                && doc.check_disk(&key.0) != state::DiskCheck::Nothing
            {
                n += 1;
            }
        }
        // A tab that couldn't open its file tries again.
        self.failed.retain(|k, _| !hit(&k.0));
        n
    }

    /// Check every open file (the window came back into focus: events may have been missed).
    pub fn check_all_disk(&mut self) -> usize {
        let paths: Vec<PathBuf> = self.docs.keys().map(|k| k.0.clone()).collect();
        self.check_disk(&paths)
    }

    /// Files under `from` were renamed to `to`: their buffers move along (edits and undo kept).
    /// Chat views stay: a rename on disk doesn't move a chat's pending changes.
    pub fn rename_path(&mut self, from: &Path, to: &Path) {
        let moved: Vec<DocKey> = self
            .docs
            .keys()
            .filter(|k| k.1 == EditorView::Disk && k.0.starts_with(from))
            .cloned()
            .collect();
        for key in moved {
            let path = moved_path(&key.0, from, to);
            if let Some(mut doc) = self.docs.remove(&key) {
                doc.set_path(&path);
                self.docs.insert((path, key.1), doc);
            }
        }
    }

    /// Drop a buffer and its edits.
    pub fn discard(&mut self, key: &DocKey) {
        self.docs.remove(key);
        self.failed.remove(key);
    }

    /// Drop clean buffers no tab shows any more. Dirty ones stay, so reopening the file brings
    /// the edits back.
    pub fn retain_shown(&mut self, shown: &HashMap<DocKey, usize>) {
        self.docs
            .retain(|k, d| shown.contains_key(k) || d.core.buffer.is_dirty());
        self.failed.retain(|k, _| shown.contains_key(k));
    }

    /// Install finished parses and start the ones that are due.
    pub fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(p) = self.rx.try_recv() {
            if let Some(doc) = self.docs.get_mut(&p.key) {
                doc.parse_done(p.version, p.tree);
            }
        }
        for (key, doc) in &mut self.docs {
            if !doc.needs_parse() {
                continue;
            }
            if let Some(t) = doc.core.edited {
                let since = t.elapsed();
                if since < PARSE_DEBOUNCE {
                    ctx.request_repaint_after(PARSE_DEBOUNCE - since);
                    continue;
                }
            }
            let version = doc.core.buffer.version();
            doc.parse_in_flight = Some(version);
            spawn_parse(
                self.tx.clone(),
                ctx.clone(),
                key.clone(),
                doc.lang().clone(),
                doc.core.buffer.rope().clone(),
                version,
            );
        }
    }
}

/// Where `path` (at or under `from`) is after `from` was renamed to `to`.
pub fn moved_path(path: &Path, from: &Path, to: &Path) -> PathBuf {
    match path.strip_prefix(from) {
        Ok(rest) if rest.as_os_str().is_empty() => to.to_path_buf(),
        Ok(rest) => to.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

/// Parse on a thread, and load the highlighter's lazy tables (tree-sitter query, syntect
/// syntax set) there too, so the UI thread never waits for them.
fn spawn_parse(
    tx: mpsc::Sender<Parsed>,
    ctx: egui::Context,
    key: DocKey,
    lang: Lang,
    rope: ropey::Rope,
    version: u64,
) {
    std::thread::spawn(move || {
        let tree = state::has_grammar(&lang)
            .then(|| Syntax::new(lang.clone()).parse_full(&rope))
            .flatten();
        let mut warm = Syntax::new(lang);
        if let Some(t) = &tree {
            warm.set_tree(t.clone(), version);
        }
        let _ = warm.highlight(&rope, 0..1);
        let _ = tx.send(Parsed { key, version, tree });
        ctx.request_repaint();
    });
}

/// Human name of a language for the status line.
pub fn lang_name(lang: &Lang) -> String {
    match lang {
        Lang::Rust => "Rust",
        Lang::TypeScript => "TypeScript",
        Lang::Tsx => "TSX",
        Lang::JavaScript => "JavaScript",
        Lang::Json => "JSON",
        Lang::Toml => "TOML",
        Lang::Yaml => "YAML",
        Lang::Markdown => "Markdown",
        Lang::Css => "CSS",
        Lang::Html => "HTML",
        Lang::Bash => "Bash",
        Lang::Python => "Python",
        Lang::C => "C",
        Lang::Cpp => "C++",
        Lang::Other(name) => name,
        Lang::Plain => "Plain text",
    }
    .to_string()
}

/// The tab icon for a file: code or text.
pub fn lang_icon(path: &Path) -> &'static str {
    match Lang::for_path(path) {
        Lang::Plain | Lang::Markdown => icon::FILE_TEXT,
        _ => icon::FILE_CODE,
    }
}

/// An editor tab's body.
pub fn show(ui: &mut Ui, p: &mut Panels, tab: u32, path: &Path, view: &EditorView) {
    let pal = theme::palette_of(ui);
    let key: DocKey = (path.to_path_buf(), view.clone());
    if p.editors.doc(&key).is_none() && p.editors.failure(&key).is_none() {
        let _ = p.editors.open(&key);
    }
    if let Some(err) = p.editors.failure(&key).map(str::to_string) {
        message(ui, &pal, icon::WARNING, &err);
        ui.vertical_centered(|ui| {
            ui.horizontal(|ui| {
                if ui
                    .button(theme::itext(icon::ARROW_CLOCKWISE, "Retry"))
                    .clicked()
                {
                    p.editors.retry(&key);
                }
                if ui
                    .button(theme::itext(icon::X_CIRCLE, "Close tab"))
                    .clicked()
                {
                    p.editors
                        .actions
                        .push(EditorAction::Close { tab, force: true });
                }
            });
        });
        return;
    }
    draw::editor(ui, p, tab, &key);
}

fn message(ui: &mut Ui, pal: &theme::Palette, ic: &str, text: &str) {
    ui.add_space(24.0);
    ui.vertical_centered(|ui| {
        ui.label(theme::ico(ic).size(30.0).color(pal.text_faint));
        ui.label(RichText::new(text).color(pal.text_dim));
    });
}
