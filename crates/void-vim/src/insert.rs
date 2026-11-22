//! Insert and Replace mode (`:help insert.txt`): typing, the editing keys, and the session that
//! dot-repeat and a count replay.

use crate::engine::{Change, Engine};
use crate::keys::char_of;
use crate::parse::{Act, Cmd, Ins};
use crate::registers::Register;
use crate::text::*;
use crate::{Mode, VimHost};

/// One key or one piece of typed text, as an insert session saw it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum InsKey {
    Key(String),
    Text(String),
}

/// The insert session between entering Insert/Replace mode and `<Esc>`.
#[derive(Debug)]
pub(crate) struct Session {
    /// `3ifoo<Esc>` inserts three times.
    count: usize,
    /// Each extra repetition opens a new line first (`3o`).
    open_line: bool,
    keys: Vec<InsKey>,
    /// Replace mode: what each typed char overwrote (`None` when it was appended), for `<BS>`.
    replaced: Vec<Option<char>>,
    /// The command that started the session, for dot-repeat.
    change: Option<Cmd>,
    /// After `<C-r>`, waiting for a register name.
    ctrl_r: bool,
}

impl Engine {
    pub(crate) fn start_insert(
        &mut self,
        mode: Mode,
        change: Option<Cmd>,
        count: usize,
        open_line: bool,
    ) {
        self.state.mode = mode;
        self.insert = Some(Session {
            count,
            open_line,
            keys: Vec::new(),
            replaced: Vec::new(),
            change,
            ctrl_r: false,
        });
    }

    /// `i a I A o O` and `R`.
    pub(crate) fn begin_insert(
        &mut self,
        h: &mut dyn VimHost,
        cmd: &Cmd,
        ins: Option<Ins>,
    ) -> bool {
        let cur = h.selection().1;
        self.begin(h);
        let r = h.rope();
        let line = line_of(r, cur);
        let pos = match ins {
            None | Some(Ins::Before) => cur,
            Some(Ins::After) => (cur + 1).min(line_end(r, line)),
            Some(Ins::LineStart) => line_start(r, line) + indent_len(r, line),
            Some(Ins::LineEnd) => line_end(r, line),
            Some(Ins::Below) => {
                self.open_below(h);
                h.selection().1
            }
            Some(Ins::Above) => {
                let indent = indent(r, line);
                let start = line_start(r, line);
                self.edit(h, start..start, &format!("{indent}\n"));
                start + indent.chars().count()
            }
        };
        self.set_cursor(h, pos);
        let mode = if ins.is_none() {
            Mode::Replace
        } else {
            Mode::Insert
        };
        let open = matches!(ins, Some(Ins::Below | Ins::Above));
        self.start_insert(mode, Some(cmd.clone()), cmd.count.unwrap_or(1), open);
        true
    }

    fn open_below(&mut self, h: &mut dyn VimHost) {
        let r = h.rope();
        let end = line_end(r, line_of(r, h.selection().1));
        h.select(end, end);
        self.host_insert(h, |h| h.newline());
    }

    /// A key in Insert or Replace mode. Returns whether it was used.
    pub(crate) fn insert_key(&mut self, h: &mut dyn VimHost, key: &str) -> bool {
        let Some(s) = self.insert.as_mut() else {
            return false;
        };
        if s.ctrl_r {
            s.ctrl_r = false;
            if let Some(name) = char_of(key)
                && let Some(reg) = self.read_register(h, Some(name))
            {
                self.typed_text(h, &reg.text);
            }
            return true;
        }
        let record = |s: &mut Session| s.keys.push(InsKey::Key(key.to_string()));
        let replace = self.state.mode == Mode::Replace;
        let cur = h.selection().1;
        match key {
            "<Esc>" | "<C-c>" => self.finish_insert(h, key == "<Esc>"),
            "<C-r>" => s.ctrl_r = true,
            "<CR>" | "<C-j>" => {
                record(s);
                if replace {
                    s.replaced.push(None);
                    self.edit(h, cur..cur, "\n");
                    self.set_cursor(h, cur + 1);
                } else {
                    self.host_insert(h, |h| h.newline());
                }
            }
            "<Tab>" => {
                record(s);
                self.host_insert(h, |h| h.tab());
            }
            "<BS>" => {
                record(s);
                self.backspace(h);
            }
            "<Del>" => {
                record(s);
                if cur < h.rope().len_chars() {
                    self.edit(h, cur..cur + 1, "");
                }
            }
            "<C-w>" | "<C-u>" => {
                record(s);
                let r = h.rope();
                let line = line_of(r, cur);
                let start = line_start(r, line);
                let from = if cur == start {
                    cur.saturating_sub(1)
                } else if key == "<C-u>" {
                    let indent = start + indent_len(r, line);
                    if cur > indent { indent } else { start }
                } else {
                    let mut p = cur;
                    while p > start && is_blank(r.char(p - 1)) {
                        p -= 1;
                    }
                    if p > start {
                        let k = class(r.char(p - 1), false);
                        while p > start && class(r.char(p - 1), false) == k {
                            p -= 1;
                        }
                    }
                    p
                };
                self.edit(h, from..cur, "");
                self.set_cursor(h, from);
            }
            "<Left>" | "<Right>" | "<Up>" | "<Down>" | "<Home>" | "<End>" => {
                let r = h.rope();
                let line = line_of(r, cur);
                let (start, end) = (line_start(r, line), line_end(r, line));
                let c = cur - start;
                let pos = match key {
                    "<Left>" => cur.saturating_sub(1).max(start),
                    "<Right>" => (cur + 1).min(end),
                    "<Home>" => start,
                    "<End>" => end,
                    "<Up>" if line > 0 => (line_start(r, line - 1) + c).min(line_end(r, line - 1)),
                    "<Down>" if line < last_line(r) => {
                        (line_start(r, line + 1) + c).min(line_end(r, line + 1))
                    }
                    _ => cur,
                };
                if pos != cur {
                    // Moving the cursor starts a new undo step and a new insert to repeat, and
                    // ends a block insert without copying it.
                    self.block_insert = None;
                    self.end(h);
                    self.set_cursor(h, pos);
                    self.begin(h);
                    let s = self.insert.as_mut().expect("in insert");
                    s.keys.clear();
                    s.replaced.clear();
                    s.count = 1;
                    s.open_line = false;
                    s.change = s.change.as_ref().map(|_| Cmd {
                        reg: None,
                        count: None,
                        act: if replace {
                            Act::Replace
                        } else {
                            Act::Insert(Ins::Before)
                        },
                    });
                }
            }
            k => match char_of(k) {
                Some(c) => {
                    record(s);
                    self.type_char(h, c);
                }
                None => return false,
            },
        }
        true
    }

    /// Text typed or pasted in one go (also `<C-r>{reg}`).
    pub(crate) fn typed_text(&mut self, h: &mut dyn VimHost, text: &str) {
        let Some(s) = self.insert.as_mut() else {
            return;
        };
        s.keys.push(InsKey::Text(text.to_string()));
        if self.state.mode == Mode::Replace {
            for c in text.chars() {
                self.type_char(h, c);
            }
        } else {
            let cur = h.selection().1;
            self.edit(h, cur..cur, text);
            self.set_cursor(h, cur + text.chars().count());
        }
    }

    fn type_char(&mut self, h: &mut dyn VimHost, c: char) {
        let cur = h.selection().1;
        let r = h.rope();
        let mut buf = [0; 4];
        let text = c.encode_utf8(&mut buf);
        if self.state.mode == Mode::Replace {
            let over = (c != '\n' && cur < line_end(r, line_of(r, cur))).then(|| r.char(cur));
            if let Some(s) = self.insert.as_mut() {
                s.replaced.push(over);
            }
            let end = if over.is_some() { cur + 1 } else { cur };
            self.edit(h, cur..end, text);
        } else {
            self.edit(h, cur..cur, text);
        }
        self.set_cursor(h, cur + 1);
    }

    fn backspace(&mut self, h: &mut dyn VimHost) {
        let cur = h.selection().1;
        if cur == 0 {
            return;
        }
        if self.state.mode == Mode::Replace {
            match self.insert.as_mut().and_then(|s| s.replaced.pop()) {
                Some(Some(old)) => self.edit(h, cur - 1..cur, &old.to_string()),
                Some(None) => self.edit(h, cur - 1..cur, ""),
                // Before where Replace mode started: only move.
                None if cur == line_start(h.rope(), line_of(h.rope(), cur)) => return,
                None => {}
            }
        } else {
            self.edit(h, cur - 1..cur, "");
        }
        self.set_cursor(h, cur - 1);
    }

    fn replay_insert(&mut self, h: &mut dyn VimHost, key: &InsKey) {
        match key {
            InsKey::Key(k) => {
                self.insert_key(h, k);
            }
            InsKey::Text(t) => self.typed_text(h, t),
        }
    }

    /// Replay the insertion of a dot-repeated change, then leave Insert mode.
    pub(crate) fn replay_session(&mut self, h: &mut dyn VimHost, keys: &[InsKey]) {
        for k in keys {
            self.replay_insert(h, k);
        }
        if self.insert.is_some() {
            self.insert_key(h, "<Esc>");
        }
    }

    /// `<Esc>`: repeat for a count, close the undo step, remember the change, step left.
    fn finish_insert(&mut self, h: &mut dyn VimHost, repeat: bool) {
        let Some(s) = self.insert.as_ref() else {
            return;
        };
        let (count, open_line, keys) = (s.count, s.open_line, s.keys.clone());
        if repeat {
            for _ in 1..count {
                if open_line {
                    self.open_below(h);
                }
                for k in &keys {
                    self.replay_insert(h, k);
                }
            }
        }
        let s = self.insert.take().expect("in insert");
        let mut text = String::new();
        for k in &keys {
            match k {
                InsKey::Text(t) => text.push_str(t),
                InsKey::Key(k) if k == "<CR>" => text.push('\n'),
                InsKey::Key(k) if k == "<BS>" => {
                    text.pop();
                }
                InsKey::Key(k) => text.extend(char_of(k)),
            }
        }
        self.registers.set(
            '.',
            Register {
                text,
                linewise: false,
            },
        );
        if let Some(cmd) = s.change {
            self.last_change = Some(Change { cmd, insert: keys });
        }
        self.state.mode = Mode::Normal;
        let r = h.rope();
        let cur = h.selection().1;
        let pos = match self.block_insert.take() {
            Some(b) if repeat => self.finish_block_insert(h, b, cur),
            _ if cur > line_start(r, line_of(r, cur)) => cur - 1,
            _ => cur,
        };
        self.set_cursor(h, pos);
        self.end(h);
    }
}
