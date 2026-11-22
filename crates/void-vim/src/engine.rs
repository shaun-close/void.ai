//! The engine: modes, pending keys, dispatch, dot-repeat (`:help repeat.txt`), macros, Visual
//! mode (`:help visual.txt`) and scrolling (`:help scroll.txt`).

use std::collections::VecDeque;

use crate::insert::{InsKey, Session};
use crate::keys::{canon, char_of, decode, encode};
use crate::motion::{Find, Motion, Want as ReachWant};
use crate::operator::{Region, extent_region, motion_region, visual_extent, visual_region};
use crate::parse::{Act, Cmd, Fail, Op, Scroll, Target, parse};
use crate::registers::{Register, Registers};
use crate::text::*;
use crate::textobj::object;
use crate::{ScrollAt, VimHost};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Normal,
    Insert,
    Replace,
    Visual,
    VisualLine,
    /// Entered with `<C-v>`; block operators come with the visual-block work.
    VisualBlock,
}

impl Mode {
    pub fn is_visual(self) -> bool {
        matches!(self, Mode::Visual | Mode::VisualLine | Mode::VisualBlock)
    }
}

/// What the UI shows: the mode, keys typed so far, the register being recorded, a message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    pub mode: Mode,
    pub pending: String,
    pub recording: Option<char>,
    pub message: String,
}

/// The remembered column for `j`/`k` (`curswant`), and the head it was remembered at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Curswant {
    Column(usize),
    End,
}

/// The last change, for `.`: the command and what was typed after it.
#[derive(Debug, Clone)]
pub(crate) struct Change {
    pub cmd: Cmd,
    pub insert: Vec<InsKey>,
}

/// A line the engine is waiting for: from the host's prompt, or from replayed keys.
#[derive(Debug, Clone)]
pub(crate) enum Prompt {
    Ex {
        lines: Option<(usize, usize)>,
    },
    Search {
        forward: bool,
        count: Option<usize>,
        op: Option<(Op, Option<char>)>,
    },
}

/// Most keys one top-level key may set off through macros before the engine gives up.
const KEY_BUDGET: usize = 1_000_000;

#[derive(Default)]
pub struct Engine {
    pub(crate) state: State,
    pub(crate) registers: Registers,
    pending: Vec<String>,
    pub(crate) want: Option<(Curswant, usize)>,
    pub(crate) last_find: Option<(Find, char)>,
    pub(crate) last_search: Option<(String, bool)>,
    pub(crate) last_change: Option<Change>,
    pub(crate) insert: Option<Session>,
    recording: Option<String>,
    last_play: Option<char>,
    queue: VecDeque<String>,
    replaying: bool,
    pub(crate) failed: bool,
    group_depth: usize,
    prompt: Option<Prompt>,
    cmdline: Option<(Prompt, String)>,
    pub(crate) last_visual: Option<(Mode, usize, usize)>,
    scroll_amount: Option<usize>,
}

impl Engine {
    pub fn new() -> Engine {
        Engine::default()
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn registers(&self) -> &Registers {
        &self.registers
    }

    /// Feed one key (a Vim key name). Returns whether the engine used it; keys it has no use for
    /// (`<C-s>`, `<F5>`, …) are left to the app.
    pub fn key(&mut self, host: &mut impl VimHost, key: &str) -> bool {
        let h: &mut dyn VimHost = host;
        self.state.message.clear();
        let key = canon(key).into_owned();
        let was_recording = self.recording.is_some();
        self.failed = false;
        let used = self.feed(h, &key);
        if used
            && was_recording
            && let Some(rec) = self.recording.as_mut()
        {
            encode(&key, rec);
        }
        self.drain(h);
        self.settle(h);
        used
    }

    /// Text from an input method or a paste. In Insert/Replace mode it goes in as it is; in other
    /// modes each character is a key.
    pub fn typed(&mut self, host: &mut impl VimHost, text: &str) {
        if !matches!(self.state.mode, Mode::Insert | Mode::Replace) {
            for c in text.chars() {
                let key = match c {
                    '\n' => "<CR>".to_string(),
                    '\t' => "<Tab>".to_string(),
                    c => c.to_string(),
                };
                self.key(host, &key);
            }
            return;
        }
        let h: &mut dyn VimHost = host;
        if let Some(rec) = self.recording.as_mut() {
            rec.push_str(text);
        }
        self.typed_text(h, text);
        self.settle(h);
    }

    /// The line the host collected after [`VimHost::prompt`], or `None` when it was cancelled.
    pub fn prompt_answer(&mut self, host: &mut impl VimHost, answer: Option<String>) {
        let h: &mut dyn VimHost = host;
        self.state.message.clear();
        let Some(prompt) = self.prompt.take() else {
            return;
        };
        if let Some(rec) = self.recording.as_mut() {
            match &answer {
                Some(text) => {
                    rec.push_str(text);
                    rec.push('\r');
                }
                None => rec.push('\x1b'),
            }
        }
        self.failed = false;
        self.answer(h, prompt, answer);
        self.drain(h);
        self.settle(h);
    }

    /// Run keys queued by macros, stopping them all when a command fails.
    fn drain(&mut self, h: &mut dyn VimHost) {
        let mut budget = KEY_BUDGET;
        while let Some(k) = self.queue.pop_front() {
            self.replaying = true;
            self.failed = false;
            self.feed(h, &k);
            budget -= 1;
            if budget == 0 {
                self.state.message = "Macro stopped: too many keys".into();
                self.queue.clear();
            }
            if self.failed {
                self.queue.clear();
            }
        }
        self.replaying = false;
        self.state.pending = self.pending.join("");
    }

    /// Keep the selection consistent with the mode after every entry point.
    fn settle(&mut self, h: &mut dyn VimHost) {
        let r = h.rope();
        let (anchor, head) = h.selection();
        let len = r.len_chars();
        match self.state.mode {
            Mode::Normal => {
                let p = clamp_normal(r, head.min(len));
                if (anchor, head) != (p, p) {
                    h.select(p, p);
                }
            }
            Mode::Insert | Mode::Replace => {
                if (anchor, head) != (head.min(len), head.min(len)) {
                    h.select(head.min(len), head.min(len));
                }
            }
            _ => {
                if anchor > len || head > len {
                    h.select(anchor.min(len), head.min(len));
                }
            }
        }
    }

    fn feed(&mut self, h: &mut dyn VimHost, key: &str) -> bool {
        if let Some((_, text)) = self.cmdline.as_mut() {
            match key {
                "<CR>" | "<C-j>" => {
                    let (p, text) = self.cmdline.take().expect("cmdline");
                    self.answer(h, p, Some(text));
                }
                "<Esc>" | "<C-c>" => self.cmdline = None,
                "<BS>" => {
                    if text.pop().is_none() {
                        self.cmdline = None;
                    }
                }
                k => text.extend(char_of(k)),
            }
            return true;
        }
        match self.state.mode {
            Mode::Insert | Mode::Replace => self.insert_key(h, key),
            _ => self.normal_key(h, key),
        }
    }

    fn normal_key(&mut self, h: &mut dyn VimHost, key: &str) -> bool {
        if matches!(key, "<Esc>" | "<C-c>") {
            if !self.pending.is_empty() {
                self.pending.clear();
            } else if self.state.mode.is_visual() {
                self.exit_visual(h);
            }
            self.state.pending.clear();
            return true;
        }
        self.pending.push(key.to_string());
        let visual = self.state.mode.is_visual();
        match parse(&self.pending, visual, self.recording.is_some()) {
            Err(Fail::More) => {
                self.state.pending = self.pending.join("");
                true
            }
            Err(Fail::Bad) => {
                let alone = self.pending.len() == 1;
                self.pending.clear();
                self.state.pending.clear();
                if alone && key.starts_with('<') && key.len() > 1 {
                    return false;
                }
                self.failed = true;
                true
            }
            Ok(cmd) => {
                self.pending.clear();
                self.state.pending.clear();
                self.exec(h, cmd);
                true
            }
        }
    }

    pub(crate) fn begin(&mut self, h: &mut dyn VimHost) {
        if self.group_depth == 0 {
            h.begin_group();
        }
        self.group_depth += 1;
    }

    pub(crate) fn end(&mut self, h: &mut dyn VimHost) {
        self.group_depth = self.group_depth.saturating_sub(1);
        if self.group_depth == 0 {
            h.end_group();
        }
    }

    /// Move the head (keeping the Visual anchor in Visual modes).
    pub(crate) fn set_cursor(&mut self, h: &mut dyn VimHost, pos: usize) {
        if self.state.mode.is_visual() {
            let anchor = h.selection().0;
            h.select(anchor, pos);
        } else {
            h.select(pos, pos);
        }
    }

    fn changed(&mut self, cmd: Cmd) {
        if self.insert.is_none() {
            self.last_change = Some(Change {
                cmd,
                insert: Vec::new(),
            });
        }
    }

    /// Run a complete command.
    pub(crate) fn exec(&mut self, h: &mut dyn VimHost, cmd: Cmd) {
        let n = cmd.count.unwrap_or(1);
        let visual = self.state.mode.is_visual();
        let ok = match cmd.act.clone() {
            Act::Move(m) => self.do_move(h, &m, cmd.count),
            Act::Op(op, target) => self.do_op(h, &cmd, op, &target),
            Act::OpSearch(op, forward) => {
                self.open_prompt(
                    h,
                    Prompt::Search {
                        forward,
                        count: cmd.count,
                        op: Some((op, cmd.reg)),
                    },
                );
                true
            }
            Act::Insert(ins) => self.begin_insert(h, &cmd, Some(ins)),
            Act::Replace => self.begin_insert(h, &cmd, None),
            Act::Paste { before, .. } if visual => self.visual_put(h, &cmd, before),
            Act::Paste { before, after } => {
                let ok = self.put(h, &cmd, before, after);
                if ok {
                    self.changed(cmd.clone());
                }
                ok
            }
            Act::Join { spaces } => {
                let r = h.rope();
                let (line, count) = if visual {
                    let (a, b) = h.selection();
                    let (l1, l2) = (line_of(r, a.min(b)), line_of(r, a.max(b)));
                    self.exit_visual(h);
                    (l1, l2 - l1 + 1)
                } else {
                    (line_of(r, h.selection().1), n)
                };
                let ok = self.join(h, line, count, spaces);
                if ok {
                    self.changed(Cmd {
                        reg: None,
                        count: Some(count),
                        act: Act::Join { spaces },
                    });
                }
                ok
            }
            Act::ReplaceChar(c) if visual => self.visual_replace(h, c),
            Act::ReplaceChar(c) => {
                let ok = self.replace_chars(h, c, n);
                if ok {
                    self.changed(cmd.clone());
                }
                ok
            }
            Act::ToggleCase => {
                let ok = self.toggle_chars(h, n);
                if ok {
                    self.changed(cmd.clone());
                }
                ok
            }
            Act::Repeat => self.dot(h, cmd.count),
            Act::Undo => {
                (0..n).for_each(|_| h.undo());
                true
            }
            Act::Redo => {
                (0..n).for_each(|_| h.redo());
                true
            }
            Act::Visual(mode) => {
                self.visual(h, mode);
                true
            }
            Act::Reselect => match self.last_visual {
                Some((mode, anchor, head)) => {
                    let len = h.rope().len_chars();
                    self.state.mode = mode;
                    h.select(anchor.min(len), head.min(len));
                    true
                }
                None => false,
            },
            Act::Search { forward } => {
                self.open_prompt(
                    h,
                    Prompt::Search {
                        forward,
                        count: cmd.count,
                        op: None,
                    },
                );
                true
            }
            Act::Ex => {
                let lines = visual.then(|| {
                    let r = h.rope();
                    let (a, b) = h.selection();
                    (line_of(r, a.min(b)), line_of(r, a.max(b)))
                });
                if visual {
                    self.exit_visual(h);
                }
                self.open_prompt(h, Prompt::Ex { lines });
                true
            }
            Act::Record(r) => {
                self.recording = Some(String::new());
                self.state.recording = Some(r);
                true
            }
            Act::StopRecord => {
                let text = self.recording.take().unwrap_or_default();
                if let Some(r) = self.state.recording.take() {
                    let reg = Register {
                        text,
                        linewise: false,
                    };
                    if r.is_ascii_uppercase() {
                        let mut old = self.registers.get(r).cloned().unwrap_or_default();
                        old.text.push_str(&reg.text);
                        self.registers.set(r.to_ascii_lowercase(), old);
                    } else {
                        self.registers.set(r, reg);
                    }
                }
                true
            }
            Act::Play(r) => self.play(h, r, n),
            Act::Scroll(s) => self.scroll(h, s, cmd.count),
            Act::VisualOp(op) => self.visual_op(h, &cmd, op, false),
            Act::VisualLinesOp(op) => self.visual_op(h, &cmd, op, true),
            Act::VisualObject(obj) => {
                let r = h.rope();
                match object(r, h.selection().1, obj, n) {
                    Some(Region::Chars(range)) if !range.is_empty() => {
                        self.state.mode = Mode::Visual;
                        h.select(range.start, range.end - 1);
                        true
                    }
                    Some(Region::Lines(l1, l2)) => {
                        let (a, b) = (line_start(r, l1), line_start(r, l2));
                        self.state.mode = Mode::VisualLine;
                        h.select(a, b);
                        true
                    }
                    _ => false,
                }
            }
            Act::VisualSwap => {
                let (a, b) = h.selection();
                h.select(b, a);
                true
            }
            Act::VisualRepeat(op, extent) => {
                let region = extent_region(h.rope(), h.selection().1, extent);
                let ok = self.apply(h, &cmd, op, region, n);
                if ok {
                    self.changed(cmd.clone());
                }
                ok
            }
        };
        if !ok {
            self.failed = true;
        }
    }

    fn do_move(&mut self, h: &mut dyn VimHost, m: &Motion, count: Option<usize>) -> bool {
        let cur = h.selection().1;
        let Some(reach) = self.reach(h, m, count, false) else {
            return false;
        };
        let r = h.rope();
        let pos = if self.state.mode.is_visual() {
            reach.pos.min(r.len_chars())
        } else {
            clamp_normal(r, reach.pos)
        };
        let want = match reach.want {
            ReachWant::Keep => Some((
                match self.want {
                    Some((w, at)) if at == cur => w,
                    _ => Curswant::Column(col(r, cur)),
                },
                pos,
            )),
            ReachWant::End => Some((Curswant::End, pos)),
            ReachWant::Col => None,
        };
        self.set_cursor(h, pos);
        self.want = want;
        true
    }

    fn do_op(&mut self, h: &mut dyn VimHost, cmd: &Cmd, op: Op, target: &Target) -> bool {
        let cur = h.selection().1;
        let n = cmd.count.unwrap_or(1);
        let region = match target {
            Target::Lines => {
                let r = h.rope();
                let l = line_of(r, cur);
                Region::Lines(l, (l + n - 1).min(last_line(r)))
            }
            Target::Object(obj) => match object(h.rope(), cur, *obj, n) {
                Some(region) => region,
                None => return false,
            },
            Target::Motion(Motion::WordStart { big })
                if op == Op::Change
                    && char_at(h.rope(), cur).is_some_and(|c| class(c, *big) != 0) =>
            {
                // `cw` is `ce`, but stays inside a word the cursor is at the end of.
                let r = h.rope();
                let end = line_end(r, line_of(r, cur));
                let mut p = cur;
                let k = class(r.char(p), *big);
                while p + 1 < end && class(r.char(p + 1), *big) == k {
                    p += 1;
                }
                for _ in 1..n {
                    p = crate::motion::word_end(r, p, *big);
                }
                Region::Chars(cur..p + 1)
            }
            Target::Motion(m) => {
                let Some(reach) = self.reach(h, m, cmd.count, true) else {
                    return false;
                };
                motion_region(h.rope(), cur, reach)
            }
        };
        self.want = None;
        let ok = self.apply(h, cmd, op, region, 1);
        if ok && op != Op::Yank {
            self.changed(cmd.clone());
        }
        ok
    }

    fn visual(&mut self, h: &mut dyn VimHost, mode: Mode) {
        if self.state.mode == mode {
            self.exit_visual(h);
        } else if self.state.mode.is_visual() {
            self.state.mode = mode;
        } else {
            let cur = h.selection().1;
            self.state.mode = mode;
            h.select(cur, cur);
        }
    }

    pub(crate) fn exit_visual(&mut self, h: &mut dyn VimHost) {
        let (anchor, head) = h.selection();
        self.last_visual = Some((self.state.mode, anchor, head));
        self.state.mode = Mode::Normal;
        h.select(head, head);
    }

    fn visual_op(&mut self, h: &mut dyn VimHost, cmd: &Cmd, op: Op, lines: bool) -> bool {
        if self.state.mode == Mode::VisualBlock {
            return self.block_op(h, cmd, op);
        }
        let (anchor, head) = h.selection();
        let mode = if lines {
            Mode::VisualLine
        } else {
            self.state.mode
        };
        let r = h.rope();
        let region = visual_region(r, mode, anchor, head);
        let extent = visual_extent(r, mode, anchor, head);
        self.exit_visual(h);
        let start = anchor.min(head);
        h.select(start, start);
        // Run it as the repeatable form, so a `c` session remembers that too.
        let repeat = Cmd {
            reg: cmd.reg,
            count: cmd.count,
            act: Act::VisualRepeat(op, extent),
        };
        let ok = self.apply(h, &repeat, op, region, cmd.count.unwrap_or(1));
        if ok && op != Op::Yank {
            self.changed(repeat);
        }
        ok
    }

    /// Visual-block operators: not built yet, so they do nothing rather than act on the wrong
    /// text.
    fn block_op(&mut self, h: &mut dyn VimHost, _cmd: &Cmd, _op: Op) -> bool {
        self.state.message = "Visual-block operators are not supported yet".into();
        self.exit_visual(h);
        false
    }

    /// `.`: run the last change again, with `count` replacing its count, as one undo step.
    fn dot(&mut self, h: &mut dyn VimHost, count: Option<usize>) -> bool {
        let Some(change) = self.last_change.clone() else {
            return false;
        };
        let mut cmd = change.cmd;
        if count.is_some() {
            cmd.count = count;
        }
        self.begin(h);
        self.exec(h, cmd);
        if self.insert.is_some() {
            self.replay_session(h, &change.insert);
        }
        self.end(h);
        !self.failed
    }

    /// `@r`: queue a register's keys `n` times (`@@` is the last one, `@:` the last command line).
    fn play(&mut self, h: &mut dyn VimHost, r: char, n: usize) -> bool {
        let Some(r) = (if r == '@' { self.last_play } else { Some(r) }) else {
            return false;
        };
        self.last_play = Some(r);
        let Some(reg) = self.read_register(h, Some(r)) else {
            return false;
        };
        let keys = if r == ':' {
            let mut k = vec![":".to_string()];
            k.extend(reg.text.chars().map(|c| c.to_string()));
            k.push("<CR>".into());
            k
        } else {
            decode(&reg.text)
        };
        if keys.is_empty() {
            return true;
        }
        let n = n.min(KEY_BUDGET / keys.len()).max(1);
        for _ in 0..n {
            for k in keys.iter().rev() {
                self.queue.push_front(k.clone());
            }
        }
        true
    }

    pub(crate) fn open_prompt(&mut self, h: &mut dyn VimHost, p: Prompt) {
        if self.replaying {
            self.cmdline = Some((p, String::new()));
            return;
        }
        let prefix = match p {
            Prompt::Ex { .. } => ':',
            Prompt::Search { forward: true, .. } => '/',
            Prompt::Search { .. } => '?',
        };
        self.prompt = Some(p);
        h.prompt(prefix);
    }

    fn answer(&mut self, h: &mut dyn VimHost, p: Prompt, answer: Option<String>) {
        let Some(text) = answer else {
            return;
        };
        match p {
            Prompt::Ex { lines } => {
                self.registers.set(
                    ':',
                    Register {
                        text: text.clone(),
                        linewise: false,
                    },
                );
                self.ex(h, &text, lines);
            }
            Prompt::Search { forward, count, op } => {
                let Some(pattern) = self.search_pattern(&text, forward) else {
                    self.failed = true;
                    return;
                };
                let m = Motion::Search { pattern, forward };
                let cmd = match op {
                    Some((op, reg)) => Cmd {
                        reg,
                        count,
                        act: Act::Op(op, Target::Motion(m)),
                    },
                    None => Cmd {
                        reg: None,
                        count,
                        act: Act::Move(m),
                    },
                };
                self.exec(h, cmd);
            }
        }
    }

    fn scroll(&mut self, h: &mut dyn VimHost, s: Scroll, count: Option<usize>) -> bool {
        let r = h.rope();
        let last = last_line(r);
        let cur = h.selection().1;
        let line = line_of(r, cur);
        let (top, bottom) = h.visible_lines();
        let top = top.min(last);
        let height = bottom.saturating_sub(top) + 1;
        let n = count.unwrap_or(1);
        let page = n * height.saturating_sub(2).max(1);
        let (view, at, cursor_line, first_col) = match s {
            Scroll::Top
            | Scroll::Center
            | Scroll::Bottom
            | Scroll::TopFirst
            | Scroll::CenterFirst
            | Scroll::BottomFirst => {
                let l = count.map_or(line, |c| (c.max(1) - 1).min(last));
                let at = match s {
                    Scroll::Top | Scroll::TopFirst => ScrollAt::Top,
                    Scroll::Center | Scroll::CenterFirst => ScrollAt::Center,
                    _ => ScrollAt::Bottom,
                };
                let first = matches!(
                    s,
                    Scroll::TopFirst | Scroll::CenterFirst | Scroll::BottomFirst
                );
                (l, at, Some(l), first)
            }
            Scroll::HalfDown | Scroll::HalfUp => {
                if count.is_some() {
                    self.scroll_amount = count;
                }
                let amount = self.scroll_amount.unwrap_or((height / 2).max(1));
                if s == Scroll::HalfDown {
                    if line == last {
                        return false;
                    }
                    let view = (top + amount).min(last);
                    (view, ScrollAt::Top, Some((line + amount).min(last)), true)
                } else {
                    if line == 0 {
                        return false;
                    }
                    let view = top.saturating_sub(amount);
                    (view, ScrollAt::Top, Some(line.saturating_sub(amount)), true)
                }
            }
            Scroll::PageDown | Scroll::LineDown => {
                if top >= last {
                    return false;
                }
                let step = if s == Scroll::PageDown { page } else { n };
                let view = (top + step).min(last);
                (
                    view,
                    ScrollAt::Top,
                    (line < view).then_some(view),
                    s == Scroll::PageDown,
                )
            }
            Scroll::PageUp | Scroll::LineUp => {
                if top == 0 {
                    return false;
                }
                let step = if s == Scroll::PageUp { page } else { n };
                let view = top.saturating_sub(step);
                let view_bottom = view + height - 1;
                (
                    view,
                    ScrollAt::Top,
                    (line > view_bottom).then_some(view_bottom),
                    s == Scroll::PageUp,
                )
            }
        };
        h.scroll_to(view, at);
        if let Some(l) = cursor_line {
            let r = h.rope();
            let pos = if first_col {
                first_non_blank(r, l)
            } else if l == line {
                cur
            } else {
                (line_start(r, l) + col(r, cur)).min(line_last(r, l))
            };
            if pos != cur {
                self.set_cursor(h, pos);
            }
        }
        true
    }
}
