//! One open buffer: the text, its highlighting, its Vim engine, the cursor and the view.

use std::ops::Range;
use std::path::Path;
use std::time::Instant;

use void_editor::{Buffer, Edit, Fingerprint, Lang, OnDisk, Span, Syntax, Tree};
use void_vim::{Engine, Mode};

/// How many lines Vim keeps above and below the cursor when the view follows it.
const SCROLL_OFF: usize = 3;
/// Rows to assume before a tab has been drawn once.
const DEFAULT_ROWS: usize = 30;

/// Which lines the tab shows. `top` and `rows` come from the last frame's scroll area;
/// `apply` asks the next frame to move the scroll area to `top`.
#[derive(Debug, Clone, Default)]
pub struct View {
    pub top: usize,
    pub rows: usize,
    pub left: f32,
    pub apply: bool,
    /// Vim scrolled on purpose this frame (`zt`, `<C-d>`, …): don't pull the view back.
    pub scrolled: bool,
}

impl View {
    pub fn rows(&self) -> usize {
        if self.rows == 0 {
            DEFAULT_ROWS
        } else {
            self.rows
        }
    }
}

/// A `:` or `/` line being typed at the bottom of the tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub prefix: char,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Msg {
    pub text: String,
    pub error: bool,
}

impl Msg {
    pub fn info(text: impl Into<String>) -> Msg {
        Msg {
            text: text.into(),
            error: false,
        }
    }

    pub fn error(text: impl Into<String>) -> Msg {
        Msg {
            text: text.into(),
            error: true,
        }
    }
}

/// Everything a Vim host edits: the buffer, the selection, the view, prompts and messages.
pub struct Core {
    pub buffer: Buffer,
    pub syntax: Syntax,
    /// `(anchor, head)` as char indices; inclusive at both ends in Visual modes.
    pub sel: (usize, usize),
    pub view: View,
    /// The search pattern to highlight (a Rust regex with inline flags).
    pub search: Option<String>,
    pub prompt: Option<Prompt>,
    pub message: Option<Msg>,
    pub indent: String,
    /// Opened with `:e` on a path that doesn't exist yet; the first save creates it.
    pub new_file: bool,
    /// When the text last changed (re-parse debounce).
    pub edited: Option<Instant>,
    /// The file compared with the buffer (banners).
    pub disk: DiskState,
    /// The outside version the user chose "Keep mine" over: not asked about again.
    pub kept: Option<Fingerprint>,
}

impl Core {
    pub fn len(&self) -> usize {
        self.buffer.rope().len_chars()
    }

    pub fn head(&self) -> usize {
        self.sel.1.min(self.len())
    }

    /// Cursor line and char column (zero-based).
    pub fn line_col(&self) -> (usize, usize) {
        let head = self.head();
        let line = self.buffer.line_of(head);
        (line, head - self.buffer.line_start(line))
    }

    /// Edits from the user are allowed; on a read-only buffer, say why not (the text can never
    /// be saved, so it isn't changed either).
    pub fn writable(&mut self) -> bool {
        if self.buffer.read_only {
            self.message = Some(Msg::error(
                "E21: Cannot make changes: this file is read-only",
            ));
            false
        } else {
            true
        }
    }

    /// Replace `range` and keep the syntax tree's positions in step.
    pub fn replace(&mut self, range: Range<usize>, text: &str) -> Edit {
        let edit = self.buffer.replace(range, text);
        self.edited(std::slice::from_ref(&edit));
        edit
    }

    pub fn edited(&mut self, edits: &[Edit]) {
        let version = self.buffer.version();
        for e in edits {
            self.syntax.edit(e, version);
        }
        self.edited = Some(Instant::now());
    }

    /// Undo one step: the cursor to restore and the edits made (for marks).
    pub fn undo(&mut self) -> Option<(usize, Vec<Edit>)> {
        let (cursor, edits) = self.buffer.undo_edits()?;
        self.edited(&edits);
        Some((cursor.min(self.len()), edits))
    }

    pub fn redo(&mut self) -> Option<(usize, Vec<Edit>)> {
        let (cursor, edits) = self.buffer.redo_edits()?;
        self.edited(&edits);
        Some((cursor.min(self.len()), edits))
    }

    /// The text a new line after `head` starts with: the line's indentation, plus one level
    /// after an opening bracket.
    pub fn newline_text(&self, head: usize) -> String {
        let r = self.buffer.rope();
        let line = r.char_to_line(head);
        let start = r.line_to_char(line);
        let indent: String = r
            .line(line)
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let mut it = r.chars_at(head);
        let mut before = None;
        for _ in start..head {
            match it.prev() {
                Some(c) if c.is_whitespace() => {}
                c => {
                    before = c;
                    break;
                }
            }
        }
        let extra = if matches!(before, Some('{' | '(' | '[')) {
            self.indent.as_str()
        } else {
            ""
        };
        format!("\n{indent}{extra}")
    }

    /// Save to `path` and say so the way Vim does. A file someone else changed since the
    /// buffer read it isn't overwritten (the banner asks instead); see [`Core::save_forced`].
    pub fn save(&mut self, path: &Path) -> Result<String, String> {
        self.write(path, false)
    }

    /// `:w!` / Overwrite: save even over outside changes.
    pub fn save_forced(&mut self, path: &Path) -> Result<String, String> {
        self.write(path, true)
    }

    fn write(&mut self, path: &Path, force: bool) -> Result<String, String> {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Check the disk now rather than trust a watcher event to have arrived (it may be
        // debounced, or the watcher may not be running at all).
        if !force
            && let Ok(OnDisk::Changed(fp)) = void_editor::check(path, self.buffer.disk.as_ref())
            && self.kept != Some(fp)
        {
            self.disk = DiskState::Changed;
            return Err(format!(
                "\"{name}\" changed on disk since it was read: Reload, or overwrite with :w!"
            ));
        }
        match self.buffer.save(path) {
            Ok(()) => {
                self.new_file = false;
                // Saved: the file holds the buffer again (it was recreated if it had gone).
                self.disk = DiskState::InSync;
                self.kept = None;
                Ok(format!("\"{name}\" {}L written", self.buffer.line_count()))
            }
            Err(e) => Err(format!("Couldn't save \"{name}\": {e}")),
        }
    }

    /// Scroll so `line` sits at `at` (Vim's `zt`/`zz`/`zb`, `<C-d>`, …).
    pub fn scroll_to(&mut self, line: usize, at: void_vim::ScrollAt) {
        let rows = self.view.rows();
        let top = match at {
            void_vim::ScrollAt::Top => line,
            void_vim::ScrollAt::Center => line.saturating_sub(rows / 2),
            void_vim::ScrollAt::Bottom => (line + 1).saturating_sub(rows),
        };
        self.view.top = top.min(self.max_top());
        self.view.apply = true;
        self.view.scrolled = true;
    }

    fn max_top(&self) -> usize {
        self.buffer.line_count().saturating_sub(self.view.rows())
    }

    /// Move the view the least so the cursor line shows, keeping a few lines around it
    /// (none when Vim just scrolled on purpose).
    pub fn ensure_visible(&mut self) {
        let rows = self.view.rows();
        let line = self.line_col().0;
        let off = if self.view.scrolled {
            0
        } else {
            SCROLL_OFF.min(rows.saturating_sub(1) / 2)
        };
        let top = self.view.top;
        let new_top = if line < top + off {
            line.saturating_sub(off)
        } else if line + off >= top + rows {
            (line + off + 1).saturating_sub(rows)
        } else {
            top
        }
        .min(
            self.max_top()
                .max(line.saturating_sub(rows.saturating_sub(1))),
        );
        if new_top != top {
            self.view.top = new_top;
            self.view.apply = true;
        }
    }
}

/// The file compared with the buffer, as far as the last watcher event showed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiskState {
    #[default]
    InSync,
    /// Someone else changed the file while the buffer had unsaved edits (Reload / Keep mine).
    Changed,
    /// The file is gone; the text stays, and saving puts it back.
    Deleted,
}

/// What [`Doc::check_disk`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskCheck {
    Nothing,
    Reloaded,
    Flagged,
}

/// Spans for some rows, as of (buffer version, tree version, rows).
type HlCache = (u64, u64, Range<usize>, Vec<Vec<Span>>);

/// An open buffer, shared by every tab on the same file and view.
pub struct Doc {
    pub core: Core,
    pub vim: Engine,
    /// Buffer version of a parse running on a thread.
    pub parse_in_flight: Option<u64>,
    /// Buffer version of the installed tree (tree-sitter languages).
    pub parsed: Option<u64>,
    /// The highlighter's tables are loaded (on the parse thread), so highlighting on the UI
    /// thread won't stall.
    pub warm: bool,
    hl_cache: Option<HlCache>,
    /// Plain editing: typing since the last other action is one undo step.
    pub typing: bool,
    /// Each tab's own scroll position (tabs on one buffer share the text and the cursor). The
    /// tab being drawn swaps its view into `core.view`.
    pub views: std::collections::HashMap<u32, View>,
    /// Chat-view saves sent to the engine and not answered yet, oldest first.
    pub chat_saves: Vec<ChatSave>,
}

/// A chat-view save in flight: its request id, the buffer version it holds, and the
/// fingerprint of the text sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatSave {
    pub request: u64,
    pub version: u64,
    pub fp: Fingerprint,
}

pub fn has_grammar(lang: &Lang) -> bool {
    !matches!(lang, Lang::Other(_) | Lang::Plain)
}

/// One level of indentation for the text: a tab when lines start with tabs, else 4 spaces.
fn detect_indent(buffer: &Buffer) -> String {
    let r = buffer.rope();
    let tabs = r
        .lines()
        .take(500)
        .filter(|l| l.len_chars() > 0)
        .map(|l| l.char(0))
        .filter(|c| *c == '\t' || *c == ' ')
        .fold(
            (0, 0),
            |(t, s), c| if c == '\t' { (t + 1, s) } else { (t, s + 1) },
        );
    if tabs.0 > tabs.1 { "\t" } else { "    " }.to_string()
}

impl Doc {
    pub fn new(path: &Path, buffer: Buffer) -> Doc {
        let lang = Lang::for_path(path);
        let warm = matches!(lang, Lang::Plain);
        let indent = detect_indent(&buffer);
        Doc {
            core: Core {
                syntax: Syntax::new(lang),
                buffer,
                sel: (0, 0),
                view: View::default(),
                search: None,
                prompt: None,
                message: None,
                indent,
                new_file: false,
                edited: None,
                disk: DiskState::InSync,
                kept: None,
            },
            vim: Engine::new(),
            parse_in_flight: None,
            parsed: None,
            warm,
            hl_cache: None,
            typing: false,
            views: Default::default(),
            chat_saves: Vec::new(),
        }
    }

    pub fn open(path: &Path) -> std::io::Result<Doc> {
        Ok(Doc::new(path, Buffer::from_file(path)?))
    }

    #[cfg(test)]
    pub fn text(&self) -> String {
        self.core.buffer.rope().to_string()
    }

    pub fn lang(&self) -> &Lang {
        self.core.syntax.lang()
    }

    /// Colours are showing (or there's nothing to colour).
    pub fn highlighted(&self) -> bool {
        self.warm && (!has_grammar(self.lang()) || self.parsed.is_some())
    }

    /// Wants a (re-)parse on a thread.
    pub fn needs_parse(&self) -> bool {
        self.parse_in_flight.is_none()
            && if has_grammar(self.lang()) {
                self.parsed != Some(self.core.buffer.version())
            } else {
                !self.warm
            }
    }

    /// A finished parse of buffer `version`: installed only when it matches the text (an older
    /// parse is dropped and the text parsed again).
    pub fn parse_done(&mut self, version: u64, tree: Option<Tree>) {
        self.parse_in_flight = None;
        self.warm = true;
        if version != self.core.buffer.version() {
            return;
        }
        // No tree (the parser gave up) still counts as done, or it would be retried forever.
        if let Some(tree) = tree {
            self.core.syntax.set_tree(tree, version);
        }
        self.parsed = Some(version);
        self.hl_cache = None;
    }

    /// Highlight spans for `rows` (one entry per row), cached until the text, tree or rows
    /// change. Empty until the parse thread has warmed the highlighter up.
    pub fn spans(&mut self, rows: Range<usize>) -> Vec<Vec<Span>> {
        if !self.warm {
            return vec![Vec::new(); rows.len()];
        }
        let (v, t) = (self.core.buffer.version(), self.core.syntax.version());
        if let Some((cv, ct, cr, spans)) = &self.hl_cache
            && (*cv, *ct, cr) == (v, t, &rows)
        {
            return spans.clone();
        }
        let spans = self
            .core
            .syntax
            .highlight(self.core.buffer.rope(), rows.clone());
        self.hl_cache = Some((v, t, rows, spans.clone()));
        spans
    }

    /// Put the cursor on the first non-blank of `line` (zero-based) and centre it.
    pub fn goto_line(&mut self, line: usize) {
        let c = &mut self.core;
        let line = line.min(c.buffer.line_count().saturating_sub(1));
        let start = c.buffer.line_start(line);
        let blank = c
            .buffer
            .rope()
            .line(line)
            .chars()
            .take_while(|ch| *ch == ' ' || *ch == '\t')
            .count()
            .min(c.buffer.line_len(line).saturating_sub(1));
        c.sel = (start + blank, start + blank);
        c.scroll_to(line, void_vim::ScrollAt::Center);
        c.view.scrolled = false;
        // Every tab on the buffer jumps there.
        let view = c.view.clone();
        for v in self.views.values_mut() {
            *v = view.clone();
        }
    }

    #[cfg(test)]
    pub fn disk_changed(&self) -> bool {
        self.core.disk == DiskState::Changed
    }

    #[cfg(test)]
    pub fn disk_deleted(&self) -> bool {
        self.core.disk == DiskState::Deleted
    }

    /// Compare the file with what the buffer last loaded or saved (a watcher event): our own
    /// saves and timestamp-only changes do nothing; a clean buffer reloads; a dirty one is
    /// flagged and never overwritten.
    pub fn check_disk(&mut self, path: &Path) -> DiskCheck {
        let last = self.core.buffer.disk;
        match void_editor::check(path, last.as_ref()) {
            Ok(OnDisk::Unchanged(fp)) => {
                self.core.buffer.disk = Some(fp);
                self.core.disk = DiskState::InSync;
                DiskCheck::Nothing
            }
            // Never on disk (a new `:e` buffer): nothing was deleted.
            Ok(OnDisk::Missing) if last.is_none() => DiskCheck::Nothing,
            Ok(OnDisk::Missing) => {
                let was = std::mem::replace(&mut self.core.disk, DiskState::Deleted);
                if was == DiskState::Deleted {
                    DiskCheck::Nothing
                } else {
                    DiskCheck::Flagged
                }
            }
            Ok(OnDisk::Changed(fp)) if self.core.kept == Some(fp) => DiskCheck::Nothing,
            Ok(OnDisk::Changed(_)) if !self.core.buffer.is_dirty() => match self.reload(path) {
                Ok(()) => {
                    self.core.message = Some(Msg::info(format!(
                        "\"{}\" changed on disk: reloaded",
                        super::file_name(path)
                    )));
                    DiskCheck::Reloaded
                }
                Err(e) => {
                    self.core.message = Some(Msg::error(format!("Couldn't reload: {e}")));
                    DiskCheck::Nothing
                }
            },
            Ok(OnDisk::Changed(_)) => {
                let was = std::mem::replace(&mut self.core.disk, DiskState::Changed);
                if was == DiskState::Changed {
                    DiskCheck::Nothing
                } else {
                    DiskCheck::Flagged
                }
            }
            Err(_) => DiskCheck::Nothing,
        }
    }

    /// "Keep mine": stop asking about the version on disk now (a later change asks again).
    pub fn keep_mine(&mut self, path: &Path) {
        self.core.kept = match void_editor::check(path, None) {
            Ok(OnDisk::Changed(fp)) => Some(fp),
            _ => None,
        };
        self.core.disk = DiskState::InSync;
    }

    /// The file was renamed: the buffer follows (highlighting changes with the extension).
    pub fn set_path(&mut self, path: &Path) {
        let lang = Lang::for_path(path);
        if &lang != self.lang() {
            self.warm = matches!(lang, Lang::Plain);
            self.core.syntax = Syntax::new(lang);
            self.parsed = None;
            self.hl_cache = None;
        }
    }

    pub fn mode(&self) -> Mode {
        self.vim.state().mode
    }

    /// Replace the text with the file on disk (keeping the cursor line), as one undo step;
    /// marks follow. The buffer is clean afterwards.
    pub fn reload(&mut self, path: &Path) -> std::io::Result<()> {
        self.reload_from(Buffer::from_file(path)?);
        Ok(())
    }

    /// Replace the text with `disk`'s (the file, or a chat's version of it); see
    /// [`Doc::reload`].
    pub fn reload_from(&mut self, disk: Buffer) {
        let c = &mut self.core;
        let (line, _) = c.line_col();
        let old_len = c.len();
        let text = disk.rope().to_string();
        if *c.buffer.rope() != text.as_str() {
            c.buffer.begin_group(c.head());
            let e = c.replace(0..old_len, &text);
            c.buffer.end_group(0);
            self.vim.adjust_marks(0, e.old_len, e.new_len);
        }
        let c = &mut self.core;
        c.buffer.line_ending = disk.line_ending;
        c.buffer.final_newline = disk.final_newline;
        c.buffer.read_only = disk.read_only;
        c.buffer.notice = disk.notice;
        c.buffer.mark_saved();
        let line = line.min(c.buffer.line_count().saturating_sub(1));
        let p = c.buffer.line_start(line);
        c.sel = (p, p);
        c.buffer.disk = disk.disk;
        c.new_file = false;
        c.disk = DiskState::InSync;
        c.kept = None;
        self.hl_cache = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> Doc {
        Doc::new(Path::new("x.rs"), Buffer::new(text))
    }

    #[test]
    fn newline_keeps_indent_and_opens_brackets() {
        let d = doc("    fn a() {\n");
        assert_eq!(d.core.newline_text(12), "\n        ");
        assert_eq!(d.core.newline_text(6), "\n    ");
    }

    #[test]
    fn indent_unit_follows_the_file() {
        assert_eq!(doc("\tx\n\ty\n").core.indent, "\t");
        assert_eq!(doc("  x\n").core.indent, "    ");
    }

    #[test]
    fn view_follows_cursor_with_margin() {
        let text: String = (0..100).map(|i| format!("{i}\n")).collect();
        let mut d = doc(&text);
        d.core.view.rows = 10;
        let p = d.core.buffer.line_start(50);
        d.core.sel = (p, p);
        d.core.ensure_visible();
        assert_eq!(d.core.view.top, 44, "cursor 3 lines above the bottom");
        let p = d.core.buffer.line_start(40);
        d.core.sel = (p, p);
        d.core.ensure_visible();
        assert_eq!(d.core.view.top, 37);
    }

    #[test]
    fn reload_keeps_line_and_cleans() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, "a\nb\nc\n").unwrap();
        let mut d = Doc::open(&p).unwrap();
        d.core.replace(0..0, "x");
        let at = d.core.buffer.line_start(2);
        d.core.sel = (at, at);
        std::fs::write(&p, "1\n2\n3\n4\n").unwrap();
        d.reload(&p).unwrap();
        assert_eq!(d.text(), "1\n2\n3\n4");
        assert!(!d.core.buffer.is_dirty());
        assert_eq!(d.core.line_col(), (2, 0));
    }
}
