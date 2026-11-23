//! The Vim engine's view of an editor tab: [`VimHost`] over a doc's [`Core`].

use std::ops::Range;
use std::path::Path;

use void_vim::{ScrollAt, VimHost};

use super::state::{Core, Msg, Prompt};
use super::{Clipboard, EditorAction};
use crate::layout::EditorView;

/// What a host call needs besides the text: where the doc lives and where app-level requests
/// (`:q`, `:e`, …) go.
pub struct Ctx<'a> {
    pub path: &'a Path,
    pub view: &'a EditorView,
    pub tab: u32,
    pub clipboard: &'a Clipboard,
    pub actions: &'a mut Vec<EditorAction>,
}

pub struct Host<'a, 'b> {
    pub core: &'a mut Core,
    pub cx: &'a mut Ctx<'b>,
}

impl Host<'_, '_> {
    fn set_head(&mut self, p: usize) {
        let p = p.min(self.core.len());
        self.core.sel = (p, p);
    }
}

impl VimHost for Host<'_, '_> {
    fn rope(&self) -> &ropey::Rope {
        self.core.buffer.rope()
    }

    fn selection(&self) -> (usize, usize) {
        self.core.sel
    }

    fn select(&mut self, anchor: usize, head: usize) {
        self.core.sel = (anchor, head);
    }

    fn replace(&mut self, range: Range<usize>, text: &str) {
        if self.core.writable() {
            self.core.replace(range, text);
        }
    }

    fn begin_group(&mut self) {
        let head = self.core.head();
        self.core.buffer.begin_group(head);
    }

    fn end_group(&mut self) {
        let head = self.core.head();
        self.core.buffer.end_group(head);
    }

    fn undo(&mut self) {
        if let Some((cursor, _)) = self.core.undo() {
            self.set_head(cursor);
        }
    }

    fn redo(&mut self) {
        if let Some((cursor, _)) = self.core.redo() {
            self.set_head(cursor);
        }
    }

    fn newline(&mut self) {
        if !self.core.writable() {
            return;
        }
        let head = self.core.head();
        let text = self.core.newline_text(head);
        self.core.replace(head..head, &text);
        self.set_head(head + text.chars().count());
    }

    fn tab(&mut self) {
        if !self.core.writable() {
            return;
        }
        let head = self.core.head();
        let unit = self.core.indent.clone();
        self.core.replace(head..head, &unit);
        self.set_head(head + unit.chars().count());
    }

    fn visible_lines(&self) -> (usize, usize) {
        let last = self.core.buffer.line_count().saturating_sub(1);
        let top = self.core.view.top.min(last);
        (top, (top + self.core.view.rows() - 1).min(last))
    }

    fn scroll_to(&mut self, line: usize, at: ScrollAt) {
        self.core.scroll_to(line, at);
    }

    fn prompt(&mut self, prefix: char) {
        self.core.prompt = Some(Prompt {
            prefix,
            text: String::new(),
        });
    }

    fn highlight(&mut self, pattern: Option<String>) {
        self.core.search = pattern;
    }

    fn save(&mut self) {
        let msg = match self.cx.view {
            EditorView::Disk => match self.core.save(self.cx.path) {
                Ok(m) => Msg::info(m),
                Err(e) => Msg::error(e),
            },
            EditorView::Chat(_) => Msg::error("Saving a chat view isn't available yet"),
        };
        self.core.message = Some(msg);
    }

    fn ex(&mut self, cmd: &str) -> bool {
        let tab = self.cx.tab;
        let (name, arg) = match cmd.split_once(char::is_whitespace) {
            Some((n, a)) => (n, a.trim()),
            None => (cmd, ""),
        };
        let action = match (name, arg) {
            ("q" | "quit" | "clo" | "close", "") => EditorAction::Close { tab, force: false },
            ("q!" | "quit!" | "clo!" | "close!", "") => EditorAction::Close { tab, force: true },
            ("qa" | "qall" | "quita" | "quitall", "") => EditorAction::CloseAll { force: false },
            ("qa!" | "qall!" | "quita!" | "quitall!", "") => EditorAction::CloseAll { force: true },
            ("wa" | "wall", "") => EditorAction::SaveAll,
            ("e" | "edit", "") => EditorAction::Reload { tab, force: false },
            ("e!" | "edit!", "") => EditorAction::Reload { tab, force: true },
            ("e" | "edit", path) => EditorAction::Open {
                path: path.into(),
                from: self.cx.path.to_path_buf(),
            },
            _ => return false,
        };
        self.cx.actions.push(action);
        true
    }

    fn clipboard_get(&self) -> Option<String> {
        self.cx.clipboard.get()
    }

    fn clipboard_set(&mut self, s: &str) {
        self.cx.clipboard.set(s);
    }

    fn indent_unit(&self) -> &str {
        &self.core.indent
    }
}

pub fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}
